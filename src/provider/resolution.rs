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
    let key = builtin_api_key(
        std::env::var("ANTHROPIC_API_KEY").ok().as_deref(),
        cfg.anthropic_api_key.as_deref(),
        "ANTHROPIC_API_KEY",
        "anthropic_api_key",
    )?;
    Ok((
        Box::new(anthropic::Anthropic::new(
            cfg.anthropic_base_url.clone(),
            key,
        )),
        model.to_string(),
    ))
}

fn openai_provider(cfg: &Config, model: &str) -> Result<(Box<dyn Provider>, String), Error> {
    let key = openai_api_key(
        std::env::var("OPENAI_API_KEY").ok().as_deref(),
        cfg.openai_api_key.as_deref(),
    )?;
    Ok((
        Box::new(openai::OpenAi::new(cfg.openai_base_url.clone(), key)),
        model.to_string(),
    ))
}

/// Resolves a built-in provider key. The environment variable wins when set;
/// otherwise the stored config value is resolved, which may itself be a
/// `$NAME` or `${NAME}` reference.
fn builtin_api_key(
    environment: Option<&str>,
    stored: Option<&str>,
    env_name: &str,
    config_key: &str,
) -> Result<String, Error> {
    if let Some(value) = environment.filter(|value| !value.trim().is_empty()) {
        return Ok(value.to_string());
    }
    match stored {
        Some(value) => crate::config::resolve_config_value(value),
        None => Err(Error::Config(format!(
            "{env_name} is not set and no {config_key} is configured; run 'yawl --setup'"
        ))),
    }
}

fn openai_api_key(environment: Option<&str>, stored: Option<&str>) -> Result<String, Error> {
    match builtin_api_key(environment, stored, "OPENAI_API_KEY", "openai_api_key") {
        Err(_) if stored.is_none() => Ok(String::new()),
        result => result,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_key_prefers_the_environment_over_stored_values() {
        let key = builtin_api_key(
            Some("env-key"),
            Some("stored-key"),
            "ANTHROPIC_API_KEY",
            "anthropic_api_key",
        )
        .expect("the environment key should win");

        assert_eq!(key, "env-key");
    }

    #[test]
    fn builtin_key_falls_back_to_the_stored_literal() {
        let key = builtin_api_key(None, Some("stored-key"), "OPENAI_API_KEY", "openai_api_key")
            .expect("the stored key should be used");

        assert_eq!(key, "stored-key");
        let error = builtin_api_key(None, None, "ANTHROPIC_API_KEY", "anthropic_api_key")
            .expect_err("a missing key should name both sources");
        assert!(
            error.to_string().contains("ANTHROPIC_API_KEY is not set"),
            "unexpected error: {error}"
        );
        assert!(
            error.to_string().contains("anthropic_api_key"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn builtin_key_resolves_environment_references() {
        let missing = builtin_api_key(
            None,
            Some("$YAWL_UNSET_REFERENCE_9F3A"),
            "OPENAI_API_KEY",
            "openai_api_key",
        )
        .expect_err("an unset reference should fail");

        assert!(
            missing
                .to_string()
                .contains("YAWL_UNSET_REFERENCE_9F3A is not set"),
            "unexpected error: {missing}"
        );
    }

    #[test]
    fn blank_environment_values_fall_through_to_the_stored_key() {
        let key = builtin_api_key(
            Some("  "),
            Some("stored-key"),
            "OPENAI_API_KEY",
            "openai_api_key",
        )
        .expect("a blank env value should not mask the stored key");

        assert_eq!(key, "stored-key");
    }

    #[test]
    fn openai_key_is_optional_only_when_unconfigured() {
        let key = openai_api_key(None, None).expect("OpenAI should allow keyless requests");
        assert!(key.is_empty());

        let error = openai_api_key(None, Some("$YAWL_UNSET_OPENAI_REFERENCE_4C71"))
            .expect_err("an explicitly configured missing reference should fail");
        assert!(
            error
                .to_string()
                .contains("YAWL_UNSET_OPENAI_REFERENCE_4C71 is not set"),
            "unexpected error: {error}"
        );
    }
}
