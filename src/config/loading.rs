use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::schema::ConfigFile;
use super::{
    Config, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_COMPACT_THRESHOLD, DEFAULT_MAX_SUBAGENTS,
    DEFAULT_MAX_TOKENS, DEFAULT_OPENAI_BASE_URL, DEFAULT_SUBAGENT_MODEL,
    DEFAULT_SUBAGENT_REQUEST_BUDGET, DEFAULT_SUBAGENT_TIMEOUT_SECS, MAX_SUBAGENT_REQUEST_BUDGET,
    MAX_SUBAGENT_TIMEOUT_SECS, ProviderConfig, UiColor,
};
use crate::error::Error;

impl Config {
    pub fn load() -> Result<Config, Error> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Self::load_from(home.join(".yawl"), PathBuf::from(".yawl"))
    }

    pub(crate) fn load_from(home_dir: PathBuf, project_dir: PathBuf) -> Result<Config, Error> {
        let mut cfg = Self::defaults(home_dir, project_dir);
        let global = cfg.global_config_path();
        let project = cfg.project_dir.join("config.json");
        for path in [global, project] {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let file: ConfigFile = serde_json::from_str(&text)
                        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                    cfg.apply(file)
                        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
            }
        }
        Ok(cfg)
    }

    fn defaults(home_dir: PathBuf, project_dir: PathBuf) -> Config {
        let home = home_dir.parent().unwrap_or(&home_dir);
        Config {
            model: None,
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            openai_base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            scroll_bar: true,
            scroll_bar_auto_hide: true,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            subagents: false,
            max_subagents: DEFAULT_MAX_SUBAGENTS,
            subagent_model: DEFAULT_SUBAGENT_MODEL.to_string(),
            subagent_request_budget: DEFAULT_SUBAGENT_REQUEST_BUDGET,
            subagent_timeout_secs: DEFAULT_SUBAGENT_TIMEOUT_SECS,
            skill_dirs: vec![home.join(".yawl/skills"), home.join(".agents/skills")],
            providers: default_local_providers(),
            setup_skipped: false,
            anthropic_api_key: None,
            openai_api_key: None,
            home_dir,
            project_dir,
        }
    }

    pub(crate) fn reload(&self) -> Result<Config, Error> {
        Self::load_from(self.home_dir.clone(), self.project_dir.clone())
    }

    /// Merges one on-disk file into the effective config. Values are held to
    /// the same rules interactive changes enforce, so a hand-edited
    /// config.json fails loudly instead of degrading silently.
    fn apply(&mut self, file: ConfigFile) -> Result<(), Error> {
        if let Some(value) = file.model {
            self.model = (!value.trim().is_empty()).then_some(value);
        }
        if let Some(value) = file.anthropic_base_url {
            self.anthropic_base_url = value;
        }
        if let Some(value) = file.openai_base_url {
            self.openai_base_url = value;
        }
        if let Some(value) = file.max_tokens {
            if value == 0 {
                return Err(Error::Config(
                    "max_tokens must be a positive integer".into(),
                ));
            }
            self.max_tokens = value;
        }
        if let Some(value) = file.reasoning_effort {
            match value.as_str() {
                "default" | "off" => self.reasoning_effort = None,
                level @ ("minimal" | "low" | "medium" | "high" | "xhigh" | "max") => {
                    self.reasoning_effort = Some(level.to_string());
                }
                _ => return Err(Error::Config("unsupported reasoning effort".into())),
            }
        }
        if let Some(value) = file.hide_reasoning {
            self.hide_reasoning = value;
        }
        if let Some(value) = file
            .accent_color
            .or(file.status_bar_color)
            .or(file.text_box_color)
        {
            self.accent_color = value;
        }
        if let Some(value) = file.scroll_bar {
            self.scroll_bar = value;
        }
        if let Some(value) = file.scroll_bar_auto_hide {
            self.scroll_bar_auto_hide = value;
        }
        if let Some(map) = file.context_windows {
            for (model, window) in &map {
                if *window == 0 {
                    return Err(Error::Config(format!(
                        "context_windows.{model} must be a positive integer"
                    )));
                }
            }
            self.context_windows.extend(map);
        }
        if let Some(value) = file.auto_compact {
            self.auto_compact = value;
        }
        if let Some(value) = file.compact_threshold {
            self.compact_threshold = validate_bounded(value, 0.1, 0.99, "compact_threshold")?;
        }
        if let Some(value) = file.subagents {
            self.subagents = value;
        }
        if let Some(value) = file.max_subagents {
            self.max_subagents = validate_bounded(value, 1, 16, "max_subagents")?;
        }
        if let Some(value) = file.subagent_model {
            let value = value.trim();
            if value.is_empty() {
                return Err(Error::Config("subagent_model must not be empty".into()));
            }
            self.subagent_model = value.to_string();
        }
        if let Some(value) = file.subagent_request_budget {
            self.subagent_request_budget = validate_bounded(
                value,
                0,
                MAX_SUBAGENT_REQUEST_BUDGET,
                "subagent_request_budget",
            )?;
        }
        if let Some(value) = file.subagent_timeout_secs {
            self.subagent_timeout_secs =
                validate_bounded(value, 0, MAX_SUBAGENT_TIMEOUT_SECS, "subagent_timeout_secs")?;
        }
        if let Some(dirs) = file.skill_dirs {
            self.skill_dirs = dirs
                .into_iter()
                .filter(|dir| !dir.trim().is_empty())
                .map(|dir| expand_home_path(&dir, &self.home_dir))
                .collect();
        }
        if let Some(value) = file.setup {
            if value != "skipped" {
                return Err(Error::Config(format!(
                    "setup must be \"skipped\" if set, found \"{value}\""
                )));
            }
            self.setup_skipped = true;
        }
        if let Some(value) = file.anthropic_api_key {
            self.anthropic_api_key = (!value.trim().is_empty()).then_some(value);
        }
        if let Some(value) = file.openai_api_key {
            self.openai_api_key = (!value.trim().is_empty()).then_some(value);
        }
        if let Some(providers) = file.providers {
            for (name, provider) in providers {
                self.providers
                    .entry(name)
                    .or_insert_with(|| ProviderConfig::openai_compatible(""))
                    .apply(provider);
            }
        }
        for (name, provider) in &self.providers {
            for model in &provider.models {
                if model.context_window == Some(0) {
                    return Err(Error::Config(format!(
                        "providers.{name}.models.{} contextWindow must be a positive integer",
                        model.id
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Validates one config JSON value with the same semantic checks used while
/// loading, without reading or merging on-disk files.
pub(crate) fn validate_file_value(value: &serde_json::Value, home_dir: &Path) -> Result<(), Error> {
    let file: ConfigFile = serde_json::from_value(value.clone())?;
    Config::defaults(home_dir.to_path_buf(), PathBuf::new()).apply(file)
}

pub(crate) fn bounded_message<T: std::fmt::Display>(field: &str, min: T, max: T) -> String {
    format!("{field} must be between {min} and {max}")
}

pub(crate) fn validate_bounded<T>(value: T, min: T, max: T, field: &str) -> Result<T, Error>
where
    T: PartialOrd + std::fmt::Display,
{
    if value < min || value > max {
        Err(Error::Config(bounded_message(field, min, max)))
    } else {
        Ok(value)
    }
}

pub(crate) fn parse_bounded<T>(raw: &str, min: T, max: T, field: &str) -> Result<T, Error>
where
    T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy,
{
    let parsed = raw
        .trim()
        .parse::<T>()
        .map_err(|_| Error::Config(bounded_message(field, min, max)))?;
    validate_bounded(parsed, min, max, field)
}

pub(crate) fn expand_home_path(raw: &str, home_dir: &Path) -> PathBuf {
    let Some(user_home) = home_dir.parent() else {
        return PathBuf::from(raw);
    };
    if raw == "~" {
        return user_home.to_path_buf();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return user_home.join(rest);
    }
    PathBuf::from(raw)
}

fn default_local_providers() -> HashMap<String, ProviderConfig> {
    let mut providers = [
        ("lmstudio", "http://localhost:1234/v1"),
        ("ollama", "http://localhost:11434/v1"),
        ("omlx", "http://localhost:8000/v1"),
        ("vllm", "http://localhost:8000/v1"),
        ("sglang", "http://localhost:30000/v1"),
    ]
    .into_iter()
    .map(|(name, url)| (name.to_string(), ProviderConfig::openai_compatible(url)))
    .collect::<HashMap<_, _>>();
    if let Some(omlx) = providers.get_mut("omlx") {
        omlx.compat.requires_reasoning_content_on_assistant_messages = Some(true);
    }
    providers
}

/// Valid OpenAI Codex reasoning efforts. `default` and `off` omit the
/// request field and let the service choose its default behavior.
pub fn normalize_reasoning_effort(value: &str) -> Option<&str> {
    match value {
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Some(value),
        "default" | "off" => None,
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn test_config() -> Config {
        Config {
            providers: default_local_providers(),
            ..Config::test_default()
        }
    }

    #[test]
    fn context_window_prefers_config_override_and_provider_metadata() -> Result<(), Error> {
        let mut cfg = test_config();
        cfg.context_windows.insert("tiny".into(), 4096);
        let file: ConfigFile = serde_json::from_value(json!({
            "providers": {
                "omlx": {
                    "baseUrl": "http://127.0.0.1:8000/v1",
                    "api": "openai-completions",
                    "models": [{
                        "id": "local-model",
                        "contextWindow": 65536,
                        "maxTokens": 4096
                    }]
                }
            }
        }))?;
        cfg.apply(file)?;

        assert_eq!(crate::model::context_window(&cfg, "tiny"), 4096);
        assert_eq!(crate::model::context_window(&cfg, "openai:tiny"), 4096);
        assert_eq!(
            crate::model::context_window(&cfg, "omlx:local-model"),
            65_536
        );
        assert_eq!(crate::model::max_tokens(&cfg, "omlx:local-model"), 4096);
        assert_eq!(
            crate::model::context_window(&cfg, "claude-sonnet-4-5"),
            200_000
        );
        assert_eq!(crate::model::context_window(&cfg, "gpt-4o"), 128_000);
        Ok(())
    }

    #[test]
    fn pi_style_provider_config_is_accepted() -> Result<(), Error> {
        let mut cfg = test_config();
        let file: ConfigFile = serde_json::from_value(json!({
            "providers": {
                "omlx": {
                    "baseUrl": "http://localhost:9000/v1",
                    "apiKey": "local-key",
                    "authHeader": true,
                    "compat": {
                        "supportsUsageInStreaming": false,
                        "maxTokensField": "max_tokens"
                    },
                    "models": [{"id": "qwen", "name": "Qwen local"}]
                }
            }
        }))?;
        cfg.apply(file)?;

        let provider = &cfg.providers["omlx"];
        assert_eq!(provider.base_url, "http://localhost:9000/v1");
        assert_eq!(provider.api_key.as_deref(), Some("local-key"));
        assert_eq!(provider.models[0].id, "qwen");
        assert!(!provider.compat.usage_in_stream());
        assert!(provider.compat.reasoning_content_on_assistant_messages());
        Ok(())
    }

    #[test]
    fn reasoning_effort_accepts_levels_and_default() -> Result<(), Error> {
        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(json!({"reasoning_effort": "high"}))?)?;
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));

        cfg.apply(serde_json::from_value(
            json!({"reasoning_effort": "default"}),
        )?)?;
        assert_eq!(cfg.reasoning_effort, None);
        Ok(())
    }

    #[test]
    fn reasoning_is_visible_unless_hidden() -> Result<(), Error> {
        let mut cfg = test_config();
        assert!(!cfg.hide_reasoning);

        cfg.apply(serde_json::from_value(json!({"hide_reasoning": true}))?)?;
        assert!(cfg.hide_reasoning);
        Ok(())
    }

    #[test]
    fn scroll_bar_is_visible_unless_disabled() -> Result<(), Error> {
        let mut cfg = test_config();
        assert!(cfg.scroll_bar);

        cfg.apply(serde_json::from_value(json!({"scroll_bar": false}))?)?;
        assert!(!cfg.scroll_bar);
        Ok(())
    }

    #[test]
    fn scroll_bar_auto_hide_is_on_unless_disabled() -> Result<(), Error> {
        let mut cfg = test_config();
        assert!(cfg.scroll_bar_auto_hide);

        cfg.apply(serde_json::from_value(
            json!({"scroll_bar_auto_hide": false}),
        )?)?;
        assert!(!cfg.scroll_bar_auto_hide);
        Ok(())
    }

    #[test]
    fn ui_colors_accept_palette_names_and_custom_rgb() -> Result<(), Error> {
        assert_eq!(
            UiColor::parse("blue")
                .expect("the built-in blue palette name should parse")
                .config_value(),
            "blue"
        );
        assert_eq!(
            UiColor::parse("#123aBc").expect("the valid RGB fixture should parse"),
            UiColor::new(0x12, 0x3a, 0xbc)
        );
        assert!(UiColor::parse("not-a-color").is_err());

        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(json!({
            "accent_color": "#102030"
        }))?)?;
        assert_eq!(cfg.accent_color, UiColor::new(0x10, 0x20, 0x30));

        cfg.apply(serde_json::from_value(
            json!({"status_bar_color": "green"}),
        )?)?;
        assert_eq!(cfg.accent_color.config_value(), "green");
        Ok(())
    }

    #[test]
    fn loaded_values_meet_interactive_validation_rules() {
        let cases = [
            (
                json!({"max_tokens": 0}),
                "max_tokens must be a positive integer",
            ),
            (
                json!({"compact_threshold": 1.5}),
                "compact_threshold must be between 0.1 and 0.99",
            ),
            (
                json!({"context_windows": {"tiny": 0}}),
                "context_windows.tiny must be a positive integer",
            ),
            (
                json!({"reasoning_effort": "extreme"}),
                "unsupported reasoning effort",
            ),
            (
                json!({"providers": {"omlx": {"models": [{"id": "m", "contextWindow": 0}]}}}),
                "providers.omlx.models.m contextWindow must be a positive integer",
            ),
        ];
        for (value, expected) in cases {
            let mut cfg = test_config();
            let file: ConfigFile =
                serde_json::from_value(value).expect("the rejection fixtures should deserialize");
            let error = cfg
                .apply(file)
                .expect_err("out-of-range values should fail validation");
            assert!(
                error.to_string().contains(expected),
                "expected {expected} in {error}"
            );
        }
    }

    #[test]
    fn load_from_reports_the_file_that_failed_validation() -> Result<(), Error> {
        let root =
            std::env::temp_dir().join(format!("yawl-config-validation-{}", std::process::id()));
        let home = root.join("home/.yawl");
        std::fs::create_dir_all(&home)?;
        std::fs::write(
            home.join("config.json"),
            r#"{"context_windows": {"tiny": 0}}"#,
        )?;

        let error = Config::load_from(home.clone(), root.join("project"))
            .expect_err("invalid config should fail to load");

        assert!(
            error
                .to_string()
                .contains(&home.join("config.json").display().to_string())
        );
        assert!(error.to_string().contains("positive integer"));
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn subagent_defaults_and_project_overrides_are_merged() -> Result<(), Error> {
        let root =
            std::env::temp_dir().join(format!("yawl-subagent-config-{}", std::process::id()));
        let home = root.join("home/.yawl");
        let project = root.join("project/.yawl");
        let _ = std::fs::remove_dir_all(&root);

        let defaults = Config::load_from(home.clone(), project.clone())?;
        assert!(!defaults.subagents);
        assert_eq!(defaults.max_subagents, DEFAULT_MAX_SUBAGENTS);
        assert_eq!(defaults.subagent_model, DEFAULT_SUBAGENT_MODEL);
        assert_eq!(
            defaults.subagent_request_budget,
            DEFAULT_SUBAGENT_REQUEST_BUDGET
        );
        assert_eq!(
            defaults.subagent_timeout_secs,
            DEFAULT_SUBAGENT_TIMEOUT_SECS
        );

        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            home.join("config.json"),
            r#"{"subagents":true,"max_subagents":8,"subagent_model":"configured","subagent_request_budget":50,"subagent_timeout_secs":120}"#,
        )?;
        std::fs::write(
            project.join("config.json"),
            r#"{"max_subagents":2,"subagent_model":"inherit","subagent_request_budget":0}"#,
        )?;

        let merged = Config::load_from(home, project)?;
        assert!(merged.subagents);
        assert_eq!(merged.max_subagents, 2);
        assert_eq!(merged.subagent_model, "inherit");
        assert_eq!(merged.subagent_request_budget, 0);
        assert_eq!(merged.subagent_timeout_secs, 120);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn subagent_settings_reject_invalid_limits_and_empty_models() {
        let cases = [
            json!({"max_subagents": 0}),
            json!({"max_subagents": 17}),
            json!({"subagent_model": "  "}),
            json!({"subagent_request_budget": 1001}),
            json!({"subagent_timeout_secs": 86_401}),
        ];
        for value in cases {
            let mut config = test_config();
            let file = serde_json::from_value(value).expect("subagent config fixture");
            assert!(config.apply(file).is_err());
        }
    }

    #[test]
    fn setup_marker_accepts_only_skipped_and_blank_keys_are_dropped() -> Result<(), Error> {
        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(json!({"setup": "skipped"}))?)?;
        assert!(cfg.setup_skipped);

        let mut cfg = test_config();
        let error = cfg
            .apply(serde_json::from_value(json!({"setup": "later"}))?)
            .expect_err("an unknown marker value should fail validation");
        assert!(
            error.to_string().contains("setup must be"),
            "unexpected error: {error}"
        );

        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(
            json!({"anthropic_api_key": "  ", "openai_api_key": ""}),
        )?)?;
        assert_eq!(cfg.anthropic_api_key, None);
        assert_eq!(cfg.openai_api_key, None);
        Ok(())
    }

    #[test]
    fn home_paths_expand_the_home_directory_itself_and_descendants() {
        let yawl_home = Path::new("/home/test/.yawl");
        assert_eq!(expand_home_path("~", yawl_home), Path::new("/home/test"));
        assert_eq!(
            expand_home_path("~/skills", yawl_home),
            Path::new("/home/test/skills")
        );
        assert_eq!(
            expand_home_path("relative", yawl_home),
            Path::new("relative")
        );
    }

    #[test]
    fn bounded_helpers_validate_and_parse_ranges() {
        assert_eq!(
            bounded_message("test", 1, 10),
            "test must be between 1 and 10"
        );
        assert_eq!(validate_bounded(5, 1, 10, "test").unwrap(), 5);
        assert_eq!(
            validate_bounded(0, 1, 10, "test").unwrap_err().to_string(),
            "config error: test must be between 1 and 10"
        );
        assert_eq!(
            validate_bounded(11, 1, 10, "test").unwrap_err().to_string(),
            "config error: test must be between 1 and 10"
        );

        assert_eq!(parse_bounded::<usize>("5", 1, 10, "test").unwrap(), 5);
        assert_eq!(
            parse_bounded::<usize>("not-a-number", 1, 10, "test")
                .unwrap_err()
                .to_string(),
            "config error: test must be between 1 and 10"
        );
        assert_eq!(
            parse_bounded::<usize>("15", 1, 10, "test")
                .unwrap_err()
                .to_string(),
            "config error: test must be between 1 and 10"
        );
    }
}
