//! Model target parsing, catalog lookup, and provider selection.

use crate::config::{Config, ModelConfig, ProviderConfig};

const CODEX_MODELS: &[(&str, &str, u64)] = &[
    ("gpt-5.3-codex-spark", "GPT-5.3 Codex Spark", 128_000),
    ("gpt-5.4", "GPT-5.4", 272_000),
    ("gpt-5.4-mini", "GPT-5.4 mini", 272_000),
    ("gpt-5.5", "GPT-5.5", 272_000),
    ("gpt-5.6-luna", "GPT-5.6 Luna", 272_000),
    ("gpt-5.6-sol", "GPT-5.6 Sol", 272_000),
    ("gpt-5.6-terra", "GPT-5.6 Terra", 272_000),
];

const STANDARD_REASONING: &[&str] = &["minimal", "low", "medium", "high"];
const XHIGH_REASONING: &[&str] = &["minimal", "low", "medium", "high", "xhigh"];
const MAX_REASONING: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Copy)]
pub(crate) enum ProviderSelection<'a> {
    Anthropic,
    OpenAi,
    Codex,
    Custom {
        name: &'a str,
        config: &'a ProviderConfig,
    },
}

/// One parsed model target. Model IDs may contain colons after the provider
/// prefix.
pub(crate) struct ModelTarget<'a> {
    spec: &'a str,
    model: &'a str,
    provider: ProviderSelection<'a>,
}

impl<'a> ModelTarget<'a> {
    pub(crate) fn parse(spec: &'a str, config: &'a Config) -> Self {
        if let Some(model) = spec.strip_prefix("anthropic:") {
            return Self {
                spec,
                model,
                provider: ProviderSelection::Anthropic,
            };
        }
        if let Some(model) = spec.strip_prefix("openai:") {
            return Self {
                spec,
                model,
                provider: ProviderSelection::OpenAi,
            };
        }
        if let Some(model) = spec.strip_prefix("openai-codex:") {
            return Self {
                spec,
                model,
                provider: ProviderSelection::Codex,
            };
        }
        if let Some((name, model)) = spec.split_once(':')
            && let Some(provider) = config.providers.get(name)
        {
            return Self {
                spec,
                model,
                provider: ProviderSelection::Custom {
                    name,
                    config: provider,
                },
            };
        }
        Self {
            spec,
            model: spec,
            provider: if spec.starts_with("claude") {
                ProviderSelection::Anthropic
            } else {
                ProviderSelection::OpenAi
            },
        }
    }

    pub(crate) fn model(&self) -> &'a str {
        self.model
    }

    pub(crate) fn provider(&self) -> ProviderSelection<'a> {
        self.provider
    }

    pub(crate) fn is_codex(&self) -> bool {
        matches!(self.provider, ProviderSelection::Codex)
    }

    fn configured_model(&self) -> Option<&'a ModelConfig> {
        let ProviderSelection::Custom { config, .. } = self.provider else {
            return None;
        };
        config
            .models
            .iter()
            .find(|candidate| candidate.id == self.model)
    }

    fn context_window(&self, config: &Config) -> u64 {
        if let Some(&window) = config
            .context_windows
            .get(self.spec)
            .or_else(|| config.context_windows.get(self.model))
        {
            return window;
        }
        if let Some(window) = self
            .configured_model()
            .and_then(|configured| configured.context_window)
        {
            return window;
        }
        if self.is_codex()
            && let Some((_, _, window)) = CODEX_MODELS.iter().find(|(id, _, _)| *id == self.model)
        {
            return *window;
        }
        if self.model.starts_with("claude") {
            200_000
        } else {
            128_000
        }
    }

    fn max_tokens(&self, config: &Config) -> u32 {
        self.configured_model()
            .and_then(|configured| configured.max_tokens)
            .filter(|limit| *limit > 0)
            .map_or(config.max_tokens, |limit| config.max_tokens.min(limit))
    }

    fn reasoning_efforts(&self) -> &'static [&'static str] {
        if !self.is_codex() {
            return &[];
        }
        match self.model {
            "gpt-5.6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra" => MAX_REASONING,
            "gpt-5.3-codex-spark" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.5" => XHIGH_REASONING,
            _ => STANDARD_REASONING,
        }
    }
}

pub(crate) fn context_window(config: &Config, spec: &str) -> u64 {
    ModelTarget::parse(spec, config).context_window(config)
}

pub(crate) fn max_tokens(config: &Config, spec: &str) -> u32 {
    ModelTarget::parse(spec, config).max_tokens(config)
}

pub(crate) fn is_codex(config: &Config, spec: &str) -> bool {
    ModelTarget::parse(spec, config).is_codex()
}

pub(crate) fn reasoning_efforts(config: &Config, spec: &str) -> &'static [&'static str] {
    ModelTarget::parse(spec, config).reasoning_efforts()
}

pub(crate) fn available_models(config: &Config) -> Vec<(String, String)> {
    let mut models = config
        .providers
        .iter()
        .flat_map(|(provider_name, provider)| {
            provider.models.iter().map(move |model| {
                (
                    format!("{provider_name}:{}", model.id),
                    model.name.clone().unwrap_or_else(|| model.id.clone()),
                )
            })
        })
        .collect::<Vec<_>>();
    models.extend(
        CODEX_MODELS
            .iter()
            .map(|(id, name, _)| (format!("openai-codex:{id}"), (*name).to_string())),
    );
    models.sort_by(|left, right| left.0.cmp(&right.0));
    models
}

pub(crate) fn codex_model_ids() -> Vec<String> {
    CODEX_MODELS
        .iter()
        .map(|(id, _, _)| (*id).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{DEFAULT_MAX_TOKENS, OpenAiCompatibility};

    fn config() -> Config {
        let mut providers = HashMap::new();
        providers.insert(
            "local".into(),
            ProviderConfig {
                base_url: "http://localhost/v1".into(),
                api: "openai-completions".into(),
                api_key: None,
                auth_header: None,
                headers: HashMap::new(),
                models: vec![ModelConfig {
                    id: "family:model".into(),
                    name: Some("Local model".into()),
                    context_window: Some(65_536),
                    max_tokens: Some(4096),
                    compat: OpenAiCompatibility::default(),
                }],
                compat: OpenAiCompatibility::default(),
            },
        );
        Config {
            model: None,
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: crate::config::UiColor::WHITE,
            scroll_bar: true,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: 0.85,
            subagents: false,
            max_subagents: crate::config::DEFAULT_MAX_SUBAGENTS,
            subagent_model: crate::config::DEFAULT_SUBAGENT_MODEL.to_string(),
            skill_dirs: Vec::new(),
            providers,
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
        }
    }

    #[test]
    fn one_target_keeps_provider_model_and_capabilities_consistent() {
        let config = config();
        let target = ModelTarget::parse("local:family:model", &config);

        assert!(matches!(
            target.provider(),
            ProviderSelection::Custom { name: "local", .. }
        ));
        assert_eq!(target.model(), "family:model");
        assert_eq!(target.context_window(&config), 65_536);
        assert_eq!(target.max_tokens(&config), 4096);
        assert!(target.reasoning_efforts().is_empty());
    }

    #[test]
    fn codex_capabilities_come_from_the_model_catalog() {
        let config = config();

        assert_eq!(context_window(&config, "openai-codex:gpt-5.4"), 272_000);
        assert!(reasoning_efforts(&config, "openai-codex:gpt-5.4").contains(&"xhigh"));
        assert!(!reasoning_efforts(&config, "openai-codex:gpt-5.4").contains(&"max"));
        assert!(reasoning_efforts(&config, "openai-codex:gpt-5.6-sol").contains(&"max"));
    }

    #[test]
    fn explicit_and_inferred_builtin_targets_match_existing_rules() {
        let config = config();

        assert!(matches!(
            ModelTarget::parse("anthropic:claude-sonnet", &config).provider(),
            ProviderSelection::Anthropic
        ));
        assert!(matches!(
            ModelTarget::parse("claude-sonnet", &config).provider(),
            ProviderSelection::Anthropic
        ));
        assert!(matches!(
            ModelTarget::parse("gpt-4o", &config).provider(),
            ProviderSelection::OpenAi
        ));
    }
}
