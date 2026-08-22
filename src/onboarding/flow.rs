//! The interactive setup wizard: provider choice, credentials, model pick.
//! Every write goes through `ConfigChange`, so wizard saves and interactive
//! settings obey the same validation and precedence rules.

use crate::config::{Config, ConfigChange};
use crate::error::Error;
use crate::provider::codex::{CodexLoginStatus, credential_status};

use super::SetupOutcome;
use super::discovery;
use super::select::{self, Choice};
use super::terminal::{self, Authentication};

/// What one provider step wants to save. `changes` excludes the default
/// model; the wizard decides whether to apply that.
struct SetupPlan {
    changes: Vec<ConfigChange>,
    summary: Vec<String>,
    default_model: String,
}

#[derive(Clone, Copy)]
enum ProviderChoice {
    KeepCurrent,
    Codex,
    Anthropic,
    OpenAi,
    Local(&'static str),
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
        let mut updated = config.clone();
        for change in changes {
            updated = updated.change_global(change)?.config;
        }
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
    menu.push((
        ProviderChoice::Codex,
        Choice::new(
            "OpenAI Codex",
            "signs in with a ChatGPT Plus or Pro account",
        ),
    ));
    menu.push((
        ProviderChoice::Anthropic,
        Choice::new("Anthropic", "Claude models with an API key"),
    ));
    menu.push((
        ProviderChoice::OpenAi,
        Choice::new("OpenAI", "GPT models with an API key"),
    ));
    menu.push((
        ProviderChoice::Local("ollama"),
        Choice::new("Ollama", "local models at 127.0.0.1:11434"),
    ));
    menu.push((
        ProviderChoice::Local("lmstudio"),
        Choice::new("LM Studio", "local models at 127.0.0.1:1234"),
    ));
    menu.push((
        ProviderChoice::Local("omlx"),
        Choice::new("OMLX", "local models at 127.0.0.1:8000"),
    ));
    menu.push((
        ProviderChoice::Custom,
        Choice::new(
            "Other OpenAI-compatible",
            "any server with /v1/chat/completions",
        ),
    ));
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
    Ok(Some(menu[index].0))
}

fn configure_provider(
    config: &Config,
    provider: ProviderChoice,
) -> Result<Option<SetupPlan>, Error> {
    match provider {
        ProviderChoice::Codex => configure_codex(config),
        ProviderChoice::Anthropic => configure_builtin(&ANTHROPIC, &config.anthropic_base_url),
        ProviderChoice::OpenAi => configure_builtin(&OPENAI, &config.openai_base_url),
        ProviderChoice::Local(name) => configure_entry(name, default_local_url(name)),
        ProviderChoice::Custom => configure_custom(config),
        // Handled by the caller before dispatch.
        ProviderChoice::KeepCurrent | ProviderChoice::Skip => Ok(None),
    }
}

struct BuiltinSpec {
    name: &'static str,
    key_environment: &'static str,
    key_config_name: &'static str,
    discover: fn(&str, &str) -> Result<Vec<String>, Error>,
    url_change: fn(String) -> ConfigChange,
    key_change: fn(String) -> ConfigChange,
    suggestions: &'static [&'static str],
}

const ANTHROPIC: BuiltinSpec = BuiltinSpec {
    name: "anthropic",
    key_environment: "ANTHROPIC_API_KEY",
    key_config_name: "anthropic_api_key",
    discover: discovery::discover_anthropic_models,
    url_change: ConfigChange::AnthropicBaseUrl,
    key_change: ConfigChange::AnthropicApiKey,
    suggestions: &["claude-sonnet-4-5", "claude-haiku-4-5"],
};

const OPENAI: BuiltinSpec = BuiltinSpec {
    name: "openai",
    key_environment: "OPENAI_API_KEY",
    key_config_name: "openai_api_key",
    discover: discovery::discover_models,
    url_change: ConfigChange::OpenAiBaseUrl,
    key_change: ConfigChange::OpenAiApiKey,
    suggestions: &[],
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

fn configure_builtin(spec: &BuiltinSpec, current_url: &str) -> Result<Option<SetupPlan>, Error> {
    let url = prompt_url("API base URL", current_url)?;
    let mut changes = vec![(spec.url_change)(url.clone())];
    let mut summary = vec![format!("{} base URL: {url}", spec.name)];

    let environment_key = std::env::var(spec.key_environment).unwrap_or_default();
    let environment_hint = if environment_key.is_empty() {
        "not set in this shell"
    } else {
        "set in this shell"
    };
    let key_choices = [
        Choice::new(
            format!("Use {} from the environment", spec.key_environment),
            environment_hint,
        ),
        Choice::new(
            "Paste the key now",
            format!("stored as {} in config.json", spec.key_config_name),
        ),
        Choice::new("Skip for now", "requests fail until a key exists"),
    ];
    let mut request_key = environment_key.clone();
    match select::select("API key", &key_choices)? {
        // Cancel and skip both leave the key sources as they are.
        None | Some(2) => {
            summary.push(format!(
                "key: {} from the environment, nothing stored",
                spec.key_environment
            ));
        }
        Some(1) => {
            let key = loop {
                let key = terminal::prompt_secret("API key")?;
                if !key.is_empty() {
                    break key;
                }
                println!("The API key must not be empty.");
            };
            changes.push((spec.key_change)(key.clone()));
            summary.push(format!("key: stored as {}", spec.key_config_name));
            request_key = key;
        }
        Some(_) => {
            summary.push(format!(
                "key: {} from the environment",
                spec.key_environment
            ));
        }
    }

    let mut models = if request_key.is_empty() {
        println!("No key available, so model discovery was skipped.");
        Vec::new()
    } else {
        match (spec.discover)(&url, &request_key) {
            Ok(models) => {
                println!("Connection OK, {} models visible.", models.len());
                models
            }
            Err(error) => {
                println!("Could not list models: {error}");
                Vec::new()
            }
        }
    };
    if models.is_empty() && !spec.suggestions.is_empty() {
        println!("Falling back to common {} models.", spec.name);
        models = spec
            .suggestions
            .iter()
            .map(|id| (*id).to_string())
            .collect();
    }
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
    configure_entry_with(&name, default_url)
}

fn configure_entry(name: &str, default_url: &str) -> Result<Option<SetupPlan>, Error> {
    configure_entry_with(name, Some(default_url.to_string()))
}

fn configure_entry_with(
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
    let Some(authentication) = terminal::choose_authentication()? else {
        return Ok(None);
    };

    let mut summary = vec![format!("providers.{name}.base_url: {url}")];
    let mut request_key = authentication.request_key().to_string();
    match &authentication {
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
        match discovery::discover_models(&url, &request_key) {
            Ok(models) => {
                println!("Connection OK, {} models visible.", models.len());
                models
            }
            Err(error) => {
                println!("Could not list models: {error}");
                Vec::new()
            }
        }
    };

    let Some(model) = choose_model(models)? else {
        return Ok(None);
    };
    let default_model = format!("{name}:{model}");
    summary.push(format!("default model: {default_model}"));
    let changes = vec![ConfigChange::Provider {
        name: name.to_string(),
        base_url: url,
        api_key: Some(authentication.config_value().to_string()),
    }];
    Ok(Some(SetupPlan {
        changes,
        summary,
        default_model,
    }))
}

fn default_local_url(name: &str) -> &'static str {
    match name {
        "ollama" => "http://127.0.0.1:11434/v1",
        "lmstudio" => "http://127.0.0.1:1234/v1",
        _ => "http://127.0.0.1:8000/v1",
    }
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
    match select::select("Model", &choices)? {
        Some(index) if index == manual => prompt_model_id().map(Some),
        Some(index) => Ok(Some(models[index].clone())),
        None => prompt_model_id().map(Some),
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
