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

const OPENAI_IMAGE_MODEL_PREFIXES: &[&str] = &[
    "chatgpt-4o",
    "gpt-4-turbo",
    "gpt-4-vision",
    "gpt-4.1",
    "gpt-4.5",
    "gpt-4o",
    "gpt-5",
    "o4-mini",
];

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

    fn supports_images(&self) -> bool {
        match self.provider {
            ProviderSelection::Anthropic => anthropic_supports_images(self.model),
            ProviderSelection::OpenAi => openai_supports_images(self.model),
            ProviderSelection::Codex => CODEX_MODELS.iter().any(|(id, _, _)| *id == self.model),
            ProviderSelection::Custom { .. } => self
                .configured_model()
                .is_some_and(|model| model.input.iter().any(|input| input == "image")),
        }
    }
}

fn anthropic_supports_images(model: &str) -> bool {
    model.starts_with("claude-3")
        || ["claude-haiku-", "claude-opus-", "claude-sonnet-"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
}

fn openai_supports_images(model: &str) -> bool {
    if model == "o1"
        || model.starts_with("o1-20")
        || model == "o3"
        || model.starts_with("o3-20")
        || model.starts_with("o3-pro")
    {
        return true;
    }
    OPENAI_IMAGE_MODEL_PREFIXES
        .iter()
        .any(|prefix| model.starts_with(prefix))
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

pub(crate) fn supports_images(config: &Config, spec: &str) -> bool {
    ModelTarget::parse(spec, config).supports_images()
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::OpenAiCompatibility;

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
                    input: vec!["text".into(), "image".into()],
                    compat: OpenAiCompatibility::default(),
                }],
                compat: OpenAiCompatibility::default(),
            },
        );
        Config {
            providers,
            ..Config::test_default()
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
        assert!(target.supports_images());
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
        assert!(supports_images(&config, "anthropic:claude-sonnet-4"));
        assert!(!supports_images(&config, "anthropic:claude-2.1"));
        assert!(supports_images(&config, "openai:gpt-4o"));
        assert!(supports_images(&config, "openai:o1"));
        assert!(!supports_images(&config, "openai:o1-mini"));
        assert!(!supports_images(&config, "openai:gpt-3.5-turbo"));
        assert!(!supports_images(&config, "openai:unknown-model"));
        assert!(supports_images(&config, "openai-codex:gpt-5.4"));
        assert!(!supports_images(&config, "openai-codex:unknown-model"));
        assert!(!supports_images(&config, "local:unlisted"));
    }
}
