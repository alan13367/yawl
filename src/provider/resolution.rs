use super::{Provider, anthropic, codex, openai};
use crate::config::Config;
use crate::error::Error;

/// Resolves a model spec to a provider instance and the bare model name.
///
/// Explicit `anthropic:` and `openai:` prefixes select the built-ins. Any
/// prefix found in `config.providers` selects that OpenAI-compatible
/// provider. Otherwise, names starting with `claude` use Anthropic and all
/// other names use the built-in OpenAI endpoint.
pub fn resolve(model_spec: &str, cfg: &Config) -> Result<(Box<dyn Provider>, String), Error> {
    let target = crate::model::ModelTarget::parse(model_spec, cfg);
    let bare = target.model();
    match target.provider() {
        crate::model::ProviderSelection::Anthropic => anthropic_provider(cfg, bare),
        crate::model::ProviderSelection::OpenAi => openai_provider(cfg, bare),
        crate::model::ProviderSelection::Codex => {
            Ok((Box::new(codex::Codex::from_config(cfg)?), bare.to_string()))
        }
        crate::model::ProviderSelection::Custom {
            name,
            config: provider,
        } => custom_provider(name, provider, bare),
    }
}

fn custom_provider(
    name: &str,
    provider: &crate::config::ProviderConfig,
    model: &str,
) -> Result<(Box<dyn Provider>, String), Error> {
    if provider.api != "openai-completions" {
        return Err(Error::Config(format!(
            "provider '{name}' uses unsupported API '{}'; Yawl supports openai-completions",
            provider.api
        )));
    }
    if provider.base_url.trim().is_empty() {
        return Err(Error::Config(format!("provider '{name}' has no base_url")));
    }

    let key = match &provider.api_key {
        Some(value) => crate::config::resolve_config_value(value)?,
        None => std::env::var(provider_key_environment_name(name)).unwrap_or_default(),
    };
    let mut headers = Vec::with_capacity(provider.headers.len());
    for (header_name, value) in &provider.headers {
        let value = crate::config::resolve_config_value(value)?;
        validate_header(header_name, &value)?;
        headers.push((header_name.clone(), value));
    }
    headers.sort_by(|left, right| left.0.cmp(&right.0));

    let mut compat = provider.compat.clone();
    if let Some(model) = provider
        .models
        .iter()
        .find(|candidate| candidate.id == model)
    {
        compat.apply(model.compat.clone());
    }
    if !matches!(
        compat.max_tokens_field(),
        "max_tokens" | "max_completion_tokens"
    ) {
        return Err(Error::Config(format!(
            "provider '{name}' has unsupported maxTokensField '{}'",
            compat.max_tokens_field()
        )));
    }
    Ok((
        Box::new(openai::OpenAi::configured(
            provider.base_url.clone(),
            key,
            provider.auth_header.unwrap_or(true),
            headers,
            compat,
        )),
        model.to_string(),
    ))
}

fn anthropic_provider(cfg: &Config, model: &str) -> Result<(Box<dyn Provider>, String), Error> {
    let key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| Error::Config("ANTHROPIC_API_KEY is not set".into()))?;
    Ok((
        Box::new(anthropic::Anthropic::new(
            cfg.anthropic_base_url.clone(),
            key,
        )),
        model.to_string(),
    ))
}

fn openai_provider(cfg: &Config, model: &str) -> Result<(Box<dyn Provider>, String), Error> {
    let key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    Ok((
        Box::new(openai::OpenAi::new(cfg.openai_base_url.clone(), key)),
        model.to_string(),
    ))
}

fn provider_key_environment_name(provider: &str) -> String {
    let mut name = provider
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    name.push_str("_API_KEY");
    name
}

fn validate_header(name: &str, value: &str) -> Result<(), Error> {
    name.parse::<ureq::http::HeaderName>()
        .map_err(|error| Error::Config(format!("invalid provider header '{name}': {error}")))?;
    value.parse::<ureq::http::HeaderValue>().map_err(|error| {
        Error::Config(format!(
            "invalid value for provider header '{name}': {error}"
        ))
    })?;
    Ok(())
}
