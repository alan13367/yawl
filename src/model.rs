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

/// Provider prefixes that route without a `providers` entry.
const BUILTIN_MODEL_PREFIXES: &[&str] = &["anthropic", "openai", "openai-codex"];

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
        if self.is_codex() {
            if let Some(window) = crate::provider::codex::cached_models(config)
                .iter()
                .find(|model| model.slug == self.model)
                .and_then(|model| model.context_window)
            {
                return window;
            }
            if let Some((_, _, window)) = CODEX_MODELS.iter().find(|(id, _, _)| *id == self.model) {
                return *window;
            }
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

    fn reasoning_efforts(&self, config: &Config) -> Vec<&'static str> {
        if let Some(model) = self.configured_model() {
            return crate::config::REASONING_EFFORTS
                .iter()
                .copied()
                .filter(|effort| model.reasoning_efforts.iter().any(|value| value == effort))
                .collect();
        }
        if !self.is_codex() {
            return Vec::new();
        }
        if let Some(model) = crate::provider::codex::cached_models(config)
            .into_iter()
            .find(|model| model.slug == self.model)
        {
            return crate::config::REASONING_EFFORTS
                .iter()
                .copied()
                .filter(|effort| {
                    model
                        .supported_reasoning_levels
                        .iter()
                        .any(|level| level.effort == *effort)
                })
                .collect();
        }
        match self.model {
            "gpt-5.6-luna" | "gpt-5.6-sol" | "gpt-5.6-terra" => MAX_REASONING,
            "gpt-5.3-codex-spark" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.5" => XHIGH_REASONING,
            _ => STANDARD_REASONING,
        }
        .to_vec()
    }

    fn saved_reasoning_efforts(&self) -> Option<Vec<&'static str>> {
        let model = self.configured_model()?;
        Some(
            crate::config::REASONING_EFFORTS
                .iter()
                .copied()
                .filter(|effort| model.reasoning_efforts.iter().any(|value| value == effort))
                .collect(),
        )
    }

    fn supports_images(&self, config: &Config) -> bool {
        match self.provider {
            ProviderSelection::Anthropic => anthropic_supports_images(self.model),
            ProviderSelection::OpenAi => openai_supports_images(self.model),
            ProviderSelection::Codex => crate::provider::codex::cached_models(config)
                .into_iter()
                .find(|model| model.slug == self.model)
                .map_or_else(
                    || CODEX_MODELS.iter().any(|(id, _, _)| *id == self.model),
                    |model| model.input_modalities.iter().any(|input| input == "image"),
                ),
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

/// Whether a model spec still routes to a provider that exists. A spec with a
/// provider prefix names a provider; anything else falls back to the built-in
/// Anthropic/OpenAI routing, which needs no configuration.
pub(crate) fn is_resolvable(config: &Config, spec: &str) -> bool {
    match spec.split_once(':') {
        Some((name, _)) if !BUILTIN_MODEL_PREFIXES.contains(&name) => {
            config.providers.contains_key(name)
        }
        _ => true,
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

pub(crate) fn reasoning_efforts(config: &Config, spec: &str) -> Vec<&'static str> {
    ModelTarget::parse(spec, config).reasoning_efforts(config)
}

/// Saved reasoning levels for a custom provider model, or `None` when the
/// spec is not a listed custom model. `Some(vec![])` is the explicit "send no
/// effort" choice and must be preserved; `None` (unknown provider or unlisted
/// model) defaults to every level in setup UI.
pub(crate) fn saved_reasoning_efforts(config: &Config, spec: &str) -> Option<Vec<&'static str>> {
    ModelTarget::parse(spec, config).saved_reasoning_efforts()
}

pub(crate) fn effective_reasoning_effort<'a>(config: &'a Config, spec: &str) -> Option<&'a str> {
    let effort = config.reasoning_effort.as_deref()?;
    reasoning_efforts(config, spec)
        .contains(&effort)
        .then_some(effort)
}

pub(crate) fn supports_images(config: &Config, spec: &str) -> bool {
    ModelTarget::parse(spec, config).supports_images(config)
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
    models.sort_by(|left, right| left.0.cmp(&right.0));
    models.extend(
        crate::provider::codex::cached_models(config)
            .into_iter()
            .map(|model| {
                let name = if model.display_name.is_empty() {
                    model.slug.clone()
                } else {
                    model.display_name
                };
                (format!("openai-codex:{}", model.slug), name)
            }),
    );
    models
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

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
                    reasoning_efforts: Vec::new(),
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
    fn available_codex_models_follow_the_saved_catalog() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = config();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        config.home_dir =
            std::env::temp_dir().join(format!("yawl-model-catalog-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&config.home_dir)?;
        std::fs::write(
            config.home_dir.join("auth.json"),
            r#"{"openai-codex":{"type":"oauth","access":"test","refresh":"test","expires":9999999999999,"accountId":"test-account"}}"#,
        )?;
        std::fs::write(
            config.home_dir.join("codex-models.json"),
            r#"{"account_id":"test-account","models":[{"slug":"gpt-6-sol","display_name":"GPT-6 Sol","visibility":"list","supported_in_api":true,"context_window":272000,"input_modalities":["text","image"],"supported_reasoning_levels":[{"effort":"low"},{"effort":"ultra"}]},{"slug":"internal","display_name":"Internal","visibility":"hide","supported_in_api":true}]}"#,
        )?;

        let models = available_models(&config);
        assert!(models.contains(&("openai-codex:gpt-6-sol".into(), "GPT-6 Sol".into())));
        assert!(!models.iter().any(|(id, _)| id == "openai-codex:internal"));
        assert!(!models.iter().any(|(id, _)| id == "openai-codex:gpt-5.4"));
        assert_eq!(context_window(&config, "openai-codex:gpt-6-sol"), 272_000);
        assert_eq!(
            reasoning_efforts(&config, "openai-codex:gpt-6-sol"),
            ["low", "ultra"]
        );
        assert!(supports_images(&config, "openai-codex:gpt-6-sol"));
        std::fs::remove_dir_all(&config.home_dir)?;
        Ok(())
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
        assert!(target.reasoning_efforts(&config).is_empty());
        assert!(target.supports_images(&config));
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
    fn custom_reasoning_uses_only_configured_levels_in_selector_order() {
        let mut config = config();
        config.providers.get_mut("local").unwrap().models[0].reasoning_efforts =
            vec!["ultra".into(), "low".into(), "low".into()];
        assert_eq!(
            reasoning_efforts(&config, "local:family:model"),
            ["low", "ultra"]
        );
        config.reasoning_effort = Some("ultra".into());
        assert_eq!(
            effective_reasoning_effort(&config, "local:family:model"),
            Some("ultra")
        );
        assert_eq!(effective_reasoning_effort(&config, "local:unlisted"), None);
        assert_eq!(
            effective_reasoning_effort(&config, "openai-codex:gpt-5.6-sol"),
            None
        );
        config.reasoning_effort = Some("high".into());
        assert_eq!(
            effective_reasoning_effort(&config, "local:family:model"),
            None
        );
        config.reasoning_effort = None;
        assert_eq!(
            effective_reasoning_effort(&config, "local:family:model"),
            None
        );
    }

    #[test]
    fn saved_reasoning_distinguishes_unknown_from_explicit_none() {
        let mut config = config();
        assert_eq!(saved_reasoning_efforts(&config, "missing:model"), None);
        assert_eq!(saved_reasoning_efforts(&config, "local:unlisted"), None);
        assert_eq!(
            saved_reasoning_efforts(&config, "local:family:model"),
            Some(vec![])
        );

        config.providers.get_mut("local").unwrap().models[0].reasoning_efforts = vec!["low".into()];
        assert_eq!(
            saved_reasoning_efforts(&config, "local:family:model"),
            Some(vec!["low"])
        );
    }

    #[test]
    fn effective_reasoning_is_limited_to_the_active_model() {
        let mut config = config();
        config.reasoning_effort = Some("high".into());

        assert_eq!(
            effective_reasoning_effort(&config, "openai-codex:gpt-5.4"),
            Some("high")
        );
        assert_eq!(
            effective_reasoning_effort(&config, "local:family:model"),
            None
        );

        config.reasoning_effort = Some("max".into());
        assert_eq!(
            effective_reasoning_effort(&config, "openai-codex:gpt-5.4"),
            None
        );
        assert_eq!(
            effective_reasoning_effort(&config, "openai-codex:gpt-5.6-sol"),
            Some("max")
        );
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
