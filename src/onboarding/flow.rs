//! The interactive setup wizard: provider choice, credentials, model pick.
//! Every write goes through `ConfigChange`, so wizard saves and interactive
//! settings obey the same validation and precedence rules.

use crate::config::{Config, ConfigChange};
use crate::error::Error;
use crate::provider::codex::{CodexLoginStatus, credential_status};

use super::SetupOutcome;
use super::provider::{self, ProviderId};
use super::select::{self, Choice};
use super::terminal::{self, Authentication};

/// What one provider step wants to save. `changes` excludes the default
/// model; the wizard decides whether to apply that.
struct SetupPlan {
    changes: Vec<ConfigChange>,
    summary: Vec<String>,
    default_model: String,
}

#[derive(Clone)]
enum ProviderChoice {
    KeepCurrent,
    Codex,
    Anthropic,
    OpenAi,
    Local(&'static str),
    Configured(String),
    Custom,
    Skip,
}

pub(super) fn wizard(config: &Config) -> Result<SetupOutcome, Error> {
    crate::set_interrupted(false);
    let mut config = config.clone();
    println!("\nWelcome to Yawl.");
    println!(
        "Changes are saved to {}.",
        config.global_config_path().display()
    );
    let mut saved_any = false;
    loop {
        let provider = match choose_provider(&config)? {
            Some(provider) => provider,
            // Esc at the menu: skip on a fresh run, keep the current setup
            // otherwise.
            None => return finish(&config),
        };
        let plan = match provider {
            ProviderChoice::KeepCurrent => return Ok(SetupOutcome::Configured(config.into())),
            ProviderChoice::Skip => return skip(&config),
            other => match configure_provider(&config, other)? {
                Some(plan) => plan,
                // The user backed out of the provider step.
                None => continue,
            },
        };

        println!(
            "\nThis will be saved to {}:",
            config.global_config_path().display()
        );
        for line in &plan.summary {
            println!("  {line}");
        }
        if !terminal::confirm("Save this setup?", true)? {
            println!("Nothing was saved.");
            continue;
        }

        let mut changes = plan.changes;
        if config.setup_skipped {
            changes.push(ConfigChange::SetupSkipped(false));
        }
        let set_default = !saved_any
            || terminal::confirm(
                &format!("Make {} the default model?", plan.default_model),
                false,
            )?;
        if set_default {
            changes.push(ConfigChange::Model(plan.default_model.clone()));
        }
        let updated = config.change_global_batch(changes)?.config;
        if updated.model.is_none() {
            return Err(Error::Config(
                "the model was saved globally but a project config overrides it; remove the empty project model setting"
                    .into(),
            ));
        }
        println!(
            "\nSaved. Default model: {}.",
            updated.model.clone().unwrap_or_default()
        );
        config = updated;
        saved_any = true;
        if !terminal::confirm("Set up another provider?", false)? {
            return Ok(SetupOutcome::Configured(config.into()));
        }
    }
}

fn finish(config: &Config) -> Result<SetupOutcome, Error> {
    if config.model.is_none() {
        skip(config)
    } else {
        Ok(SetupOutcome::Configured(config.clone().into()))
    }
}

fn skip(config: &Config) -> Result<SetupOutcome, Error> {
    config.change_global(ConfigChange::SetupSkipped(true))?;
    println!("\nSetup skipped. Run 'yawl --setup' anytime, or pass --model for one-off use.");
    Ok(SetupOutcome::Skipped)
}

fn choose_provider(config: &Config) -> Result<Option<ProviderChoice>, Error> {
    let mut menu: Vec<(ProviderChoice, Choice)> = Vec::new();
    if let Some(model) = &config.model {
        menu.push((
            ProviderChoice::KeepCurrent,
            Choice::new("Keep current setup", model.clone()),
        ));
    }
    for definition in provider::provider_catalog(config) {
        let choice = match definition.id {
            ProviderId::Codex => ProviderChoice::Codex,
            ProviderId::Anthropic => ProviderChoice::Anthropic,
            ProviderId::OpenAi => ProviderChoice::OpenAi,
            ProviderId::Compatible(name) => match name.as_str() {
                "ollama" => ProviderChoice::Local("ollama"),
                "lmstudio" => ProviderChoice::Local("lmstudio"),
                "omlx" => ProviderChoice::Local("omlx"),
                _ => ProviderChoice::Configured(name),
            },
            ProviderId::Other => ProviderChoice::Custom,
        };
        menu.push((
            choice,
            Choice::new(
                definition.label,
                if definition.configured {
                    format!("configured · {}", definition.description)
                } else {
                    definition.description
                },
            ),
        ));
    }
    if config.model.is_none() {
        menu.push((
            ProviderChoice::Skip,
            Choice::new("Skip setup", "no changes; run 'yawl --setup' anytime"),
        ));
    }
    let choices = menu
        .iter()
        .map(|(_, choice)| choice.clone())
        .collect::<Vec<_>>();
    let Some(index) = select::select("Provider", &choices)? else {
        return Ok(None);
    };
    Ok(Some(menu[index].0.clone()))
}

fn configure_provider(
    config: &Config,
    provider: ProviderChoice,
) -> Result<Option<SetupPlan>, Error> {
    match provider {
        ProviderChoice::Codex => configure_codex(config),
        ProviderChoice::Anthropic => configure_builtin(
            &ANTHROPIC,
            &config.anthropic_base_url,
            config.anthropic_api_key.as_deref(),
        ),
        ProviderChoice::OpenAi => configure_builtin(
            &OPENAI,
            &config.openai_base_url,
            config.openai_api_key.as_deref(),
        ),
        ProviderChoice::Local(name) => configure_entry(config, name, local_base_url(config, name)),
        ProviderChoice::Configured(name) => {
            let url = config
                .providers
                .get(&name)
                .map(|provider| provider.base_url.as_str())
                .unwrap_or_default();
            configure_entry(config, &name, url)
        }
        ProviderChoice::Custom => configure_custom(config),
        // Handled by the caller before dispatch.
        ProviderChoice::KeepCurrent | ProviderChoice::Skip => Ok(None),
    }
}

struct BuiltinSpec {
    name: &'static str,
    key_environment: &'static str,
    key_config_name: &'static str,
    provider: ProviderId,
    url_change: fn(String) -> ConfigChange,
    key_change: fn(Option<String>) -> ConfigChange,
}

const ANTHROPIC: BuiltinSpec = BuiltinSpec {
    name: "anthropic",
    key_environment: "ANTHROPIC_API_KEY",
    key_config_name: "anthropic_api_key",
    provider: ProviderId::Anthropic,
    url_change: ConfigChange::AnthropicBaseUrl,
    key_change: ConfigChange::AnthropicApiKey,
};

const OPENAI: BuiltinSpec = BuiltinSpec {
    name: "openai",
    key_environment: "OPENAI_API_KEY",
    key_config_name: "openai_api_key",
    provider: ProviderId::OpenAi,
    url_change: ConfigChange::OpenAiBaseUrl,
    key_change: ConfigChange::OpenAiApiKey,
};

fn configure_codex(config: &Config) -> Result<Option<SetupPlan>, Error> {
    let mut summary = Vec::new();
    if credential_status(config) == CodexLoginStatus::LoggedIn {
        let choices = [
            Choice::new(
                "Use the existing login",
                "already saved in ~/.yawl/auth.json",
            ),
            Choice::new("Sign in again", "opens OpenAI's device login"),
        ];
        match select::select("Codex login", &choices)? {
            Some(0) => summary.push("login: kept from ~/.yawl/auth.json".into()),
            None => return Ok(None),
            Some(_) => {
                crate::provider::codex::login(config)?;
                summary.push("login: saved to ~/.yawl/auth.json".into());
            }
        }
    } else {
        crate::provider::codex::login(config)?;
        summary.push("login: saved to ~/.yawl/auth.json".into());
    }

    let models: Vec<(String, String)> = crate::model::available_models(config)
        .into_iter()
        .filter(|(spec, _)| spec.starts_with("openai-codex:"))
        .collect();
    let choices = models
        .iter()
        .map(|(spec, name)| {
            Choice::new(
                spec.strip_prefix("openai-codex:").unwrap_or(spec),
                name.clone(),
            )
        })
        .collect::<Vec<_>>();
    let Some(index) = select::select("Codex model", &choices)? else {
        return Ok(None);
    };
    let model = models[index]
        .0
        .strip_prefix("openai-codex:")
        .unwrap_or(&models[index].0)
        .to_string();
    let default_model = format!("openai-codex:{model}");
    summary.push(format!("default model: {default_model}"));
    Ok(Some(SetupPlan {
        changes: Vec::new(),
        summary,
        default_model,
    }))
}

fn configure_builtin(
    spec: &BuiltinSpec,
    current_url: &str,
    stored_key: Option<&str>,
) -> Result<Option<SetupPlan>, Error> {
    let url = prompt_url("API base URL", current_url)?;
    let mut changes = vec![(spec.url_change)(url.clone())];
    let mut summary = vec![format!("{} base URL: {url}", spec.name)];

    let environment_key = std::env::var(spec.key_environment).unwrap_or_default();
    let environment_hint = if environment_key.is_empty() {
        "not set in this shell"
    } else {
        "set in this shell"
    };
    let mut key_choices = vec![
        Choice::new(
            format!("Use {} from the environment", spec.key_environment),
            environment_hint,
        ),
        Choice::new(
            "Paste the key now",
            format!("stored as {} in config.json", spec.key_config_name),
        ),
        Choice::new(
            "Keep current credential",
            if stored_key.is_some() || !environment_key.is_empty() {
                "reuse it without displaying or changing it"
            } else {
                "no credential is currently available"
            },
        ),
    ];
    if provider::can_use_no_key(&spec.provider) {
        key_choices.push(Choice::new("Use no key", "clear the saved credential"));
    }
    let mut request_key = if environment_key.is_empty() {
        stored_key
            .and_then(|value| crate::config::resolve_config_value(value).ok())
            .unwrap_or_default()
    } else {
        environment_key.clone()
    };
    match select::select("API key", &key_choices)? {
        None => return Ok(None),
        Some(2) => {
            summary.push("key: current credential preserved".into());
        }
        Some(1) => {
            let key = loop {
                let key = terminal::prompt_secret("API key")?;
                if !key.is_empty() {
                    break key;
                }
                println!("The API key must not be empty.");
            };
            changes.push((spec.key_change)(Some(key.clone())));
            summary.push(format!("key: stored as {}", spec.key_config_name));
            request_key = key;
        }
        Some(3) => {
            changes.push((spec.key_change)(None));
            summary.push("key: no API key".into());
            request_key.clear();
        }
        Some(0) => {
            request_key = environment_key;
            summary.push(format!(
                "key: {} from the environment",
                spec.key_environment
            ));
        }
        Some(_) => return Ok(None),
    }

    let models = if request_key.is_empty() {
        println!("No key available, so model discovery was skipped.");
        Vec::new()
    } else {
        let Some(models) = discover_with_recovery(|| {
            provider::discover_models(&spec.provider, &url, &request_key)
        })?
        else {
            return Ok(None);
        };
        models
    };
    let Some(model) = choose_model(models)? else {
        return Ok(None);
    };
    let default_model = format!("{}:{model}", spec.name);
    summary.push(format!("default model: {default_model}"));
    Ok(Some(SetupPlan {
        changes,
        summary,
        default_model,
    }))
}

fn configure_custom(config: &Config) -> Result<Option<SetupPlan>, Error> {
    let name = loop {
        let value = terminal::prompt("Provider name (letters, numbers, '-' or '_')")?;
        if value.is_empty() {
            println!("The name must not be empty.");
            continue;
        }
        if matches!(value.as_str(), "anthropic" | "openai") {
            println!("'{value}' is built in; pick another name.");
            continue;
        }
        if value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            break value;
        }
        println!("Use only letters, numbers, '-' and '_'.");
    };
    let configured = config
        .providers
        .get(&name)
        .map(|provider| provider.base_url.clone())
        .unwrap_or_default();
    let default_url = (!configured.is_empty()).then_some(configured);
    configure_entry_with(config, &name, default_url)
}

fn configure_entry(
    config: &Config,
    name: &str,
    default_url: &str,
) -> Result<Option<SetupPlan>, Error> {
    configure_entry_with(config, name, Some(default_url.to_string()))
}

fn configure_entry_with(
    config: &Config,
    name: &str,
    default_url: Option<String>,
) -> Result<Option<SetupPlan>, Error> {
    let url = match &default_url {
        Some(default) => prompt_url("API base URL", default)?,
        None => loop {
            let value = terminal::prompt("OpenAI-compatible API base URL, usually ending in /v1")?;
            if value.starts_with("http://") || value.starts_with("https://") {
                break value;
            }
            println!("The URL must start with http:// or https://.");
        },
    };
    let existing = config
        .providers
        .get(name)
        .and_then(|provider| provider.api_key.as_deref())
        .map(|value| crate::config::resolve_config_value(value).unwrap_or_default());
    let provider_id = ProviderId::Compatible(name.to_string());
    let Some(authentication) =
        terminal::choose_authentication(existing, provider::can_use_no_key(&provider_id))?
    else {
        return Ok(None);
    };

    let mut summary = vec![format!("providers.{name}.base_url: {url}")];
    let mut request_key = authentication.request_key().to_string();
    match &authentication {
        Authentication::Keep(_) => {
            summary.push(format!("providers.{name}: current credential preserved"));
        }
        Authentication::None => summary.push(format!("providers.{name}: no API key")),
        Authentication::Environment { reference, value } => {
            summary.push(format!("providers.{name}.api_key: {reference}"));
            if value.is_empty() {
                println!("The environment variable is not set, so model discovery was skipped.");
                request_key = String::new();
            }
        }
        Authentication::Literal(_) => {
            summary.push(format!("providers.{name}.api_key: stored in config.json"));
        }
    }

    let models = if request_key.is_empty() && !matches!(authentication, Authentication::None) {
        // Environment auth with an unset variable: nothing to send.
        Vec::new()
    } else {
        let provider = ProviderId::Compatible(name.to_string());
        let Some(models) =
            discover_with_recovery(|| provider::discover_models(&provider, &url, &request_key))?
        else {
            return Ok(None);
        };
        models
    };

    let Some(model) = choose_model(models)? else {
        return Ok(None);
    };
    let default_model = format!("{name}:{model}");
    summary.push(format!("default model: {default_model}"));
    let changes = vec![ConfigChange::Provider {
        name: name.to_string(),
        base_url: url,
        api_key: authentication.config_value().map(str::to_string),
    }];
    Ok(Some(SetupPlan {
        changes,
        summary,
        default_model,
    }))
}

fn discover_with_recovery(
    mut discover: impl FnMut() -> Result<Vec<String>, Error>,
) -> Result<Option<Vec<String>>, Error> {
    loop {
        match discover() {
            Ok(models) if !models.is_empty() => {
                println!("Connection OK, {} models visible.", models.len());
                return Ok(Some(models));
            }
            Ok(_) => println!("The provider returned no models."),
            Err(_) => println!("Could not list models from this endpoint."),
        }
        let choices = [
            Choice::new("Retry", "test the endpoint again"),
            Choice::new("Enter model manually", "continue without discovery"),
            Choice::new("Back", "choose another provider"),
        ];
        match select::select("Discovery", &choices)? {
            Some(0) => {}
            Some(1) => return Ok(Some(Vec::new())),
            Some(_) | None => return Ok(None),
        }
    }
}

fn local_base_url<'a>(config: &'a Config, name: &str) -> &'a str {
    config
        .providers
        .get(name)
        .map(|provider| provider.base_url.trim())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| provider::default_compatible_url(name))
}

/// Picks a model from `models`, or falls through to manual entry. `None`
/// means the user wants to go back to the provider menu.
fn choose_model(models: Vec<String>) -> Result<Option<String>, Error> {
    if models.is_empty() {
        return prompt_model_id().map(Some);
    }
    let mut choices = models
        .iter()
        .map(|id| Choice::new(id.clone(), ""))
        .collect::<Vec<_>>();
    let manual = choices.len();
    choices.push(Choice::new(
        "Enter a model ID manually",
        "type the exact ID the server expects",
    ));
    resolve_model_selection(&models, manual, select::select("Model", &choices)?)
}

fn resolve_model_selection(
    models: &[String],
    manual: usize,
    selection: Option<usize>,
) -> Result<Option<String>, Error> {
    match selection {
        Some(index) if index == manual => prompt_model_id().map(Some),
        Some(index) => Ok(Some(models[index].clone())),
        None => Ok(None),
    }
}

fn prompt_model_id() -> Result<String, Error> {
    loop {
        let value = terminal::prompt("Model ID")?;
        if value.is_empty() {
            println!("The model ID must not be empty.");
            continue;
        }
        return Ok(value);
    }
}

fn prompt_url(label: &str, default: &str) -> Result<String, Error> {
    loop {
        let url = terminal::prompt_with_default(label, default)?;
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(url);
        }
        println!("The URL must start with http:// or https://.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_base_url_uses_effective_config_then_builtin_default() {
        let mut config = Config::test_default();
        assert_eq!(
            local_base_url(&config, "ollama"),
            "http://127.0.0.1:11434/v1"
        );

        config.providers.insert(
            "ollama".into(),
            crate::config::ProviderConfig {
                base_url: "http://remote-host:11434/v1".into(),
                api: "openai-completions".into(),
                api_key: None,
                auth_header: None,
                headers: Default::default(),
                models: Vec::new(),
                compat: Default::default(),
            },
        );

        assert_eq!(
            local_base_url(&config, "ollama"),
            "http://remote-host:11434/v1"
        );
    }

    #[test]
    fn canceling_discovered_model_selection_returns_to_provider_menu() {
        let models = vec!["discovered-model".to_string()];

        let selected = resolve_model_selection(&models, models.len(), None)
            .expect("canceling selection should succeed");

        assert_eq!(selected, None);
    }
}
