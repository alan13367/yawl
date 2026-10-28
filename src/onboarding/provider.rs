//! Shared provider catalog and connection plan construction for setup UIs.

use crate::config::{
    Config, ConfigChange, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_OPENAI_BASE_URL, LOCAL_PROVIDERS,
    resolve_config_value,
};
use crate::error::Error;

pub(crate) use crate::config::validate_environment_name;

use super::discovery;
pub(crate) use super::discovery::DiscoveredModel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderId {
    Codex,
    Anthropic,
    OpenAi,
    Compatible(String),
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderDefinition {
    pub(crate) id: ProviderId,
    pub(crate) label: String,
    pub(crate) description: String,
    pub(crate) configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialChoice {
    Keep,
    Environment(String),
    Literal(String),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectionActivation {
    Default,
    Session,
    ConnectionOnly,
}

#[derive(Debug, Clone)]
pub(crate) struct ConnectionPlan {
    pub(crate) changes: Vec<ConfigChange>,
    pub(crate) model: String,
    pub(crate) activation: ConnectionActivation,
    pub(crate) provider_label: String,
}

impl ConnectionPlan {
    pub(crate) fn changes_for_save(&self) -> Vec<ConfigChange> {
        let mut changes = self.changes.clone();
        if self.activation == ConnectionActivation::Default {
            changes.push(ConfigChange::Model(self.model.clone()));
        }
        changes
    }

    pub(crate) fn session_model(&self) -> Option<&str> {
        (self.activation == ConnectionActivation::Session).then_some(self.model.as_str())
    }
}

pub(crate) fn provider_catalog(config: &Config) -> Vec<ProviderDefinition> {
    let mut providers = vec![
        ProviderDefinition {
            id: ProviderId::Codex,
            label: "OpenAI Codex".into(),
            description: "ChatGPT account with device login".into(),
            configured: crate::provider::codex::credential_status(config)
                == crate::provider::codex::CodexLoginStatus::LoggedIn,
        },
        builtin(
            ProviderId::Anthropic,
            "Claude models with an API key",
            config.anthropic_api_key.is_some() || std::env::var_os("ANTHROPIC_API_KEY").is_some(),
        ),
        builtin(
            ProviderId::OpenAi,
            "GPT models with an API key",
            config.openai_api_key.is_some() || std::env::var_os("OPENAI_API_KEY").is_some(),
        ),
    ];
    providers.extend(
        LOCAL_PROVIDERS
            .iter()
            .map(|(name, _, url)| compatible(config, name, url)),
    );
    let mut custom = config
        .providers
        .iter()
        .filter(|(name, _)| local_provider(name).is_none())
        .map(|(name, _provider)| ProviderDefinition {
            id: ProviderId::Compatible(name.clone()),
            label: name.clone(),
            description: "OpenAI-compatible provider".into(),
            configured: true,
        })
        .collect::<Vec<_>>();
    custom.sort_by(|left, right| left.label.cmp(&right.label));
    providers.extend(custom);
    providers.push(ProviderDefinition {
        id: ProviderId::Other,
        label: "Other OpenAI-compatible".into(),
        description: "create a named provider".into(),
        configured: false,
    });
    providers
}

fn builtin(id: ProviderId, description: &str, configured: bool) -> ProviderDefinition {
    ProviderDefinition {
        label: label(&id),
        id,
        description: description.into(),
        configured,
    }
}

/// A local server from [`LOCAL_PROVIDERS`], configured once it differs from
/// the entry every config starts with.
fn compatible(config: &Config, name: &str, default_url: &str) -> ProviderDefinition {
    let provider = config.providers.get(name);
    let address = default_url
        .trim_start_matches("http://")
        .trim_end_matches("/v1");
    let id = ProviderId::Compatible(name.into());
    ProviderDefinition {
        label: label(&id),
        id,
        description: format!("local models at {address}"),
        configured: provider.is_some_and(|provider| {
            provider.api_key.is_some()
                || !provider.models.is_empty()
                || provider.base_url != default_url
        }),
    }
}

fn local_provider(name: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    LOCAL_PROVIDERS.iter().find(|(local, _, _)| *local == name)
}

/// The display name setup and `/connect` use for `provider`.
pub(crate) fn label(provider: &ProviderId) -> String {
    match provider {
        ProviderId::Codex => "OpenAI Codex".into(),
        ProviderId::Anthropic => "Anthropic".into(),
        ProviderId::OpenAi => "OpenAI".into(),
        ProviderId::Compatible(name) => {
            local_provider(name).map_or_else(|| name.clone(), |(_, label, _)| (*label).into())
        }
        ProviderId::Other => "Other OpenAI-compatible".into(),
    }
}

/// The base URL a provider without one starts from: its local default, or
/// the usual port for a new OpenAI-compatible server.
pub(crate) fn default_compatible_url(name: &str) -> &'static str {
    local_provider(name).map_or("http://localhost:8000/v1", |(_, _, url)| url)
}

pub(crate) fn validate_provider_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err(Error::Config("provider name must not be empty".into()));
    }
    if matches!(name, "anthropic" | "openai" | "openai-codex") {
        return Err(Error::Config(format!(
            "'{name}' is a reserved provider name"
        )));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Config(
            "provider name may contain only letters, numbers, '-' and '_'".into(),
        ));
    }
    Ok(())
}

pub(crate) fn credential_environment_name(provider: &ProviderId) -> Option<String> {
    match provider {
        ProviderId::Anthropic => Some("ANTHROPIC_API_KEY".into()),
        ProviderId::OpenAi => Some("OPENAI_API_KEY".into()),
        ProviderId::Compatible(name) => Some(format!(
            "{}_API_KEY",
            name.chars()
                .map(|character| if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                })
                .collect::<String>()
        )),
        ProviderId::Codex | ProviderId::Other => None,
    }
}

/// Whether choosing no key will remain effective after the connection is
/// saved. Anthropic requires a key, and the other providers fall back to
/// their conventional environment variable when it contains a value.
pub(crate) fn can_use_no_key(provider: &ProviderId) -> bool {
    can_use_no_key_with(provider, |name| {
        std::env::var(name).is_ok_and(|value| !value.trim().is_empty())
    })
}

fn can_use_no_key_with(provider: &ProviderId, environment_has_key: impl Fn(&str) -> bool) -> bool {
    match provider {
        ProviderId::OpenAi | ProviderId::Compatible(_) => {
            credential_environment_name(provider).is_some_and(|name| !environment_has_key(&name))
        }
        ProviderId::Anthropic | ProviderId::Codex | ProviderId::Other => false,
    }
}

pub(crate) fn endpoint_change(
    provider: &ProviderId,
    base_url: String,
    credential: &CredentialChoice,
) -> Result<ConfigChange, Error> {
    let stored = credential_config_value(credential);
    match provider {
        ProviderId::Anthropic => Ok(ConfigChange::AnthropicBaseUrl(base_url)),
        ProviderId::OpenAi => Ok(ConfigChange::OpenAiBaseUrl(base_url)),
        ProviderId::Compatible(name) => Ok(ConfigChange::Provider {
            name: name.clone(),
            base_url,
            api_key: stored,
        }),
        ProviderId::Codex | ProviderId::Other => Err(Error::Config(
            "provider must be fully specified before saving".into(),
        )),
    }
}

pub(crate) fn credential_change(
    provider: &ProviderId,
    credential: &CredentialChoice,
) -> Option<ConfigChange> {
    let value = match credential {
        CredentialChoice::Keep => return None,
        CredentialChoice::Environment(name) => Some(format!("${name}")),
        CredentialChoice::Literal(value) => Some(value.clone()),
        CredentialChoice::None => None,
    };
    match provider {
        ProviderId::Anthropic => Some(ConfigChange::AnthropicApiKey(value)),
        ProviderId::OpenAi => Some(ConfigChange::OpenAiApiKey(value)),
        ProviderId::Compatible(_) | ProviderId::Codex | ProviderId::Other => None,
    }
}

fn credential_config_value(credential: &CredentialChoice) -> Option<String> {
    match credential {
        CredentialChoice::Keep => None,
        CredentialChoice::Environment(name) => Some(format!("${name}")),
        CredentialChoice::Literal(value) => Some(value.clone()),
        CredentialChoice::None => Some("-".into()),
    }
}

pub(crate) fn request_credential(
    config: &Config,
    provider: &ProviderId,
    credential: &CredentialChoice,
) -> String {
    match credential {
        CredentialChoice::Environment(name) => std::env::var(name).unwrap_or_default(),
        CredentialChoice::Literal(value) => value.clone(),
        CredentialChoice::None => String::new(),
        CredentialChoice::Keep => existing_credential(config, provider),
    }
}

pub(crate) fn credential_is_configured(config: &Config, provider: &ProviderId) -> bool {
    match provider {
        ProviderId::Anthropic => {
            config.anthropic_api_key.is_some() || std::env::var_os("ANTHROPIC_API_KEY").is_some()
        }
        ProviderId::OpenAi => {
            config.openai_api_key.is_some() || std::env::var_os("OPENAI_API_KEY").is_some()
        }
        ProviderId::Compatible(name) => config
            .providers
            .get(name)
            .is_some_and(|provider| provider.api_key.is_some()),
        ProviderId::Codex => {
            crate::provider::codex::credential_status(config)
                == crate::provider::codex::CodexLoginStatus::LoggedIn
        }
        ProviderId::Other => false,
    }
}

fn existing_credential(config: &Config, provider: &ProviderId) -> String {
    let value = match provider {
        ProviderId::Anthropic => std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .or_else(|| config.anthropic_api_key.clone()),
        ProviderId::OpenAi => std::env::var("OPENAI_API_KEY")
            .ok()
            .or_else(|| config.openai_api_key.clone()),
        ProviderId::Compatible(name) => config
            .providers
            .get(name)
            .and_then(|provider| provider.api_key.clone()),
        ProviderId::Codex | ProviderId::Other => None,
    };
    value
        .as_deref()
        .and_then(|value| resolve_config_value(value).ok())
        .unwrap_or_default()
}

pub(crate) fn discover_models(
    provider: &ProviderId,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, Error> {
    match provider {
        ProviderId::Anthropic => discovery::discover_anthropic_models(base_url, api_key),
        ProviderId::OpenAi | ProviderId::Compatible(_) => {
            discovery::discover_models(base_url, api_key)
        }
        ProviderId::Codex => Err(Error::Config(
            "Codex model selection does not use endpoint discovery".into(),
        )),
        ProviderId::Other => Err(Error::Config("provider name is not set".into())),
    }
}

pub(crate) fn default_endpoint(provider: &ProviderId, config: &Config) -> String {
    match provider {
        ProviderId::Anthropic => {
            if config.anthropic_base_url.trim().is_empty() {
                DEFAULT_ANTHROPIC_BASE_URL.into()
            } else {
                config.anthropic_base_url.clone()
            }
        }
        ProviderId::OpenAi => {
            if config.openai_base_url.trim().is_empty() {
                DEFAULT_OPENAI_BASE_URL.into()
            } else {
                config.openai_base_url.clone()
            }
        }
        ProviderId::Compatible(name) => config
            .providers
            .get(name)
            .map(|provider| provider.base_url.clone())
            .unwrap_or_else(|| default_compatible_url(name).into()),
        ProviderId::Codex | ProviderId::Other => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_providers_follow_fixed_catalog_in_sorted_order() {
        let mut config = Config::test_default();
        config
            .providers
            .insert("zeta".into(), test_provider("http://zeta.test/v1"));
        config
            .providers
            .insert("alpha".into(), test_provider("http://alpha.test/v1"));

        let labels = provider_catalog(&config)
            .into_iter()
            .map(|provider| provider.label)
            .collect::<Vec<_>>();

        assert_eq!(
            &labels[..8],
            &[
                "OpenAI Codex",
                "Anthropic",
                "OpenAI",
                "Ollama",
                "LM Studio",
                "OMLX",
                "vLLM",
                "SGLang",
            ]
        );
        assert_eq!(&labels[8..10], &["alpha", "zeta"]);
        assert_eq!(
            labels.last().map(String::as_str),
            Some("Other OpenAI-compatible")
        );
    }

    #[test]
    fn keeping_custom_credential_does_not_overwrite_it() -> Result<(), Error> {
        let change = endpoint_change(
            &ProviderId::Compatible("local".into()),
            "http://localhost:9000/v1".into(),
            &CredentialChoice::Keep,
        )?;
        assert!(matches!(
            change,
            ConfigChange::Provider { api_key: None, .. }
        ));
        Ok(())
    }

    #[test]
    fn connection_activation_controls_default_and_session_switching() {
        let base = ConnectionPlan {
            changes: vec![ConfigChange::OpenAiBaseUrl(
                "https://api.openai.com/v1".into(),
            )],
            model: "openai:gpt-test".into(),
            activation: ConnectionActivation::Default,
            provider_label: "OpenAI".into(),
        };

        assert!(matches!(
            base.changes_for_save().last(),
            Some(ConfigChange::Model(model)) if model == "openai:gpt-test"
        ));
        assert_eq!(base.session_model(), None);

        let session = ConnectionPlan {
            activation: ConnectionActivation::Session,
            ..base.clone()
        };
        assert_eq!(session.changes_for_save().len(), 1);
        assert_eq!(session.session_model(), Some("openai:gpt-test"));

        let connection_only = ConnectionPlan {
            activation: ConnectionActivation::ConnectionOnly,
            ..base
        };
        assert_eq!(connection_only.changes_for_save().len(), 1);
        assert_eq!(connection_only.session_model(), None);
    }

    #[test]
    fn no_key_is_offered_only_when_runtime_fallback_cannot_override_it() {
        assert!(!can_use_no_key_with(&ProviderId::Anthropic, |_| false));
        assert!(can_use_no_key_with(&ProviderId::OpenAi, |_| false));
        assert!(!can_use_no_key_with(&ProviderId::OpenAi, |name| {
            name == "OPENAI_API_KEY"
        }));
        assert!(!can_use_no_key_with(
            &ProviderId::Compatible("local-server".into()),
            |name| name == "LOCAL_SERVER_API_KEY"
        ));
    }

    #[test]
    fn environment_names_require_a_shell_compatible_first_character() {
        assert!(validate_environment_name("OMLX_API_KEY").is_ok());
        assert!(validate_environment_name("_PRIVATE_KEY_2").is_ok());
        assert!(validate_environment_name("2BAD").is_err());
        assert!(validate_environment_name("BAD-NAME").is_err());
    }

    #[test]
    fn trusted_project_providers_appear_in_setup_catalog() -> Result<(), Error> {
        let root = crate::test_support::unique_temp_dir("setup-project-provider");
        let project = root.join("project/.yawl");
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            project.join("config.json"),
            r#"{"providers":{"project-llm":{"base_url":"https://example.test/v1","models":[{"id":"demo"}]}}}"#,
        )?;
        let mut config = Config::load_from(root.join("home/.yawl"), project)?;
        let project_provider = ProviderId::Compatible("project-llm".into());
        assert!(
            !provider_catalog(&config)
                .iter()
                .any(|entry| entry.id == project_provider)
        );

        config.apply_project_trusted_config()?;
        assert!(
            provider_catalog(&config)
                .iter()
                .any(|entry| entry.id == project_provider)
        );
        assert_eq!(
            default_endpoint(&project_provider, &config),
            "https://example.test/v1"
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn local_endpoints_use_effective_config_then_builtin_default() {
        // A fresh config's local providers are not reported as configured.
        let fresh = Config::load_from(
            crate::test_support::unique_temp_dir("fresh-catalog").join(".yawl"),
            std::path::PathBuf::from("missing-project/.yawl"),
        )
        .expect("defaults");
        assert!(
            provider_catalog(&fresh)
                .iter()
                .filter(|entry| matches!(entry.id, ProviderId::Compatible(_)))
                .all(|entry| !entry.configured)
        );

        let mut config = Config::test_default();
        let ollama = ProviderId::Compatible("ollama".into());
        assert_eq!(
            default_endpoint(&ollama, &config),
            "http://localhost:11434/v1"
        );
        config.providers.insert(
            "ollama".into(),
            test_provider("http://remote-host:11434/v1"),
        );
        assert_eq!(
            default_endpoint(&ollama, &config),
            "http://remote-host:11434/v1"
        );
    }

    fn test_provider(base_url: &str) -> crate::config::ProviderConfig {
        crate::config::ProviderConfig::test_openai_compatible(base_url)
    }
}
