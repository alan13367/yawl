use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::schema::ConfigFile;
use super::{
    Config, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_COMPACT_THRESHOLD, DEFAULT_MAX_TOKENS,
    DEFAULT_OPENAI_BASE_URL, ProviderConfig, UiColor,
};
use crate::error::Error;

impl Config {
    pub fn load() -> Result<Config, Error> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        Self::load_from(home.join(".yawl"), PathBuf::from(".yawl"))
    }

    pub(super) fn load_from(home_dir: PathBuf, project_dir: PathBuf) -> Result<Config, Error> {
        let home = home_dir.parent().unwrap_or(&home_dir);
        let mut cfg = Config {
            model: None,
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            openai_base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            skill_dirs: vec![home.join(".yawl/skills"), home.join(".agents/skills")],
            providers: default_local_providers(),
            home_dir,
            project_dir,
        };
        let global = cfg.global_config_path();
        let project = cfg.project_dir.join("config.json");
        for path in [global, project] {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let file: ConfigFile = serde_json::from_str(&text)
                        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                    cfg.apply(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
            }
        }
        Ok(cfg)
    }

    pub(crate) fn reload(&self) -> Result<Config, Error> {
        Self::load_from(self.home_dir.clone(), self.project_dir.clone())
    }

    fn apply(&mut self, file: ConfigFile) {
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
            self.max_tokens = value.max(1);
        }
        if let Some(value) = file.reasoning_effort {
            self.reasoning_effort = normalize_reasoning_effort(&value).map(str::to_string);
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
        if let Some(map) = file.context_windows {
            self.context_windows.extend(map);
        }
        if let Some(value) = file.auto_compact {
            self.auto_compact = value;
        }
        if let Some(value) = file.compact_threshold {
            self.compact_threshold = value.clamp(0.1, 0.99);
        }
        if let Some(dirs) = file.skill_dirs {
            self.skill_dirs = dirs
                .into_iter()
                .filter(|dir| !dir.trim().is_empty())
                .map(|dir| expand_home_path(&dir, &self.home_dir))
                .collect();
        }
        if let Some(providers) = file.providers {
            for (name, provider) in providers {
                self.providers
                    .entry(name)
                    .or_insert_with(|| ProviderConfig::openai_compatible(""))
                    .apply(provider);
            }
        }
    }
}

pub(super) fn expand_home_path(value: &str, yawl_home: &Path) -> PathBuf {
    if value == "~" {
        return yawl_home.parent().unwrap_or(yawl_home).to_path_buf();
    }
    if let Some(relative) = value.strip_prefix("~/") {
        return yawl_home.parent().unwrap_or(yawl_home).join(relative);
    }
    PathBuf::from(value)
}

fn default_local_providers() -> HashMap<String, ProviderConfig> {
    let mut providers = [
        ("ollama", "http://127.0.0.1:11434/v1"),
        ("lmstudio", "http://127.0.0.1:1234/v1"),
        ("omlx", "http://127.0.0.1:8000/v1"),
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
            model: None,
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: 8192,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: 0.85,
            skill_dirs: Vec::new(),
            providers: default_local_providers(),
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
        }
    }

    #[test]
    fn context_window_prefers_config_override_and_provider_metadata()
    -> Result<(), serde_json::Error> {
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
        cfg.apply(file);

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
    fn pi_style_provider_config_is_accepted() -> Result<(), serde_json::Error> {
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
        cfg.apply(file);

        let provider = &cfg.providers["omlx"];
        assert_eq!(provider.base_url, "http://localhost:9000/v1");
        assert_eq!(provider.api_key.as_deref(), Some("local-key"));
        assert_eq!(provider.models[0].id, "qwen");
        assert!(!provider.compat.usage_in_stream());
        assert!(provider.compat.reasoning_content_on_assistant_messages());
        Ok(())
    }

    #[test]
    fn reasoning_effort_accepts_levels_and_default() -> Result<(), serde_json::Error> {
        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(json!({"reasoning_effort": "high"}))?);
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));

        cfg.apply(serde_json::from_value(
            json!({"reasoning_effort": "default"}),
        )?);
        assert_eq!(cfg.reasoning_effort, None);
        Ok(())
    }

    #[test]
    fn reasoning_is_visible_unless_hidden() -> Result<(), serde_json::Error> {
        let mut cfg = test_config();
        assert!(!cfg.hide_reasoning);

        cfg.apply(serde_json::from_value(json!({"hide_reasoning": true}))?);
        assert!(cfg.hide_reasoning);
        Ok(())
    }

    #[test]
    fn ui_colors_accept_palette_names_and_custom_rgb() -> Result<(), serde_json::Error> {
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
        }))?);
        assert_eq!(cfg.accent_color, UiColor::new(0x10, 0x20, 0x30));

        cfg.apply(serde_json::from_value(
            json!({"status_bar_color": "green"}),
        )?);
        assert_eq!(cfg.accent_color.config_value(), "green");
        Ok(())
    }
}
