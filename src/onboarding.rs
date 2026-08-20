//! First-run provider and model setup.

mod discovery;
mod terminal;

use crate::config::{Config, ConfigChange};
use crate::error::Error;
use discovery::discover_models;
use terminal::{Authentication, choose_authentication, nonempty, prompt, prompt_with_default};

#[derive(Clone, Copy)]
enum ProviderChoice {
    Ollama,
    LmStudio,
    Omlx,
    OpenAiCodex,
    Anthropic,
    OpenAi,
    Custom,
}

/// Runs first-use setup, writes the global config, then reloads the effective
/// configuration.
///
/// # Errors
///
/// Returns an error if terminal input fails, a value is invalid, provider
/// discovery fails unexpectedly, or the config cannot be saved.
pub fn run(config: &Config) -> Result<Config, Error> {
    println!(
        "Welcome to Yawl. No model is configured yet.\n\nChoose a provider:\n  1. Ollama\n  2. LM Studio\n  3. OMLX\n  4. OpenAI Codex (ChatGPT Plus/Pro)\n  5. Anthropic\n  6. OpenAI API\n  7. Other OpenAI-compatible server"
    );
    let provider = match prompt("Provider number")?.as_str() {
        "1" => ProviderChoice::Ollama,
        "2" => ProviderChoice::LmStudio,
        "3" => ProviderChoice::Omlx,
        "4" => ProviderChoice::OpenAiCodex,
        "5" => ProviderChoice::Anthropic,
        "6" => ProviderChoice::OpenAi,
        "7" => ProviderChoice::Custom,
        _ => {
            return Err(Error::Config(
                "provider must be a number from 1 through 7".into(),
            ));
        }
    };

    let model = match provider {
        ProviderChoice::Anthropic => configure_builtin(
            config,
            "anthropic",
            &config.anthropic_base_url,
            ConfigChange::AnthropicBaseUrl,
            "ANTHROPIC_API_KEY",
            false,
        )?,
        ProviderChoice::OpenAi => configure_builtin(
            config,
            "openai",
            &config.openai_base_url,
            ConfigChange::OpenAiBaseUrl,
            "OPENAI_API_KEY",
            true,
        )?,
        ProviderChoice::OpenAiCodex => {
            crate::provider::codex::login(config)?;
            format!(
                "openai-codex:{}",
                choose_model(crate::model::codex_model_ids())?
            )
        }
        ProviderChoice::Ollama => configure_local(config, "ollama", "http://127.0.0.1:11434/v1")?,
        ProviderChoice::LmStudio => {
            configure_local(config, "lmstudio", "http://127.0.0.1:1234/v1")?
        }
        ProviderChoice::Omlx => configure_local(config, "omlx", "http://127.0.0.1:8000/v1")?,
        ProviderChoice::Custom => {
            let name = prompt("Provider name, using letters, numbers, '-' or '_'")?;
            let url = prompt("OpenAI-compatible API base URL, usually ending in /v1")?;
            configure_local(config, &name, &url)?
        }
    };

    let loaded = config.change_global(ConfigChange::Model(model))?.config;
    if loaded.model.is_none() {
        return Err(Error::Config(
            "the model was saved globally but a project config overrides it; remove the empty project model setting"
                .into(),
        ));
    }
    println!(
        "\nSaved setup to {}. Starting Yawl...",
        loaded.global_config_path().display()
    );
    Ok(loaded)
}

fn configure_builtin(
    config: &Config,
    provider: &str,
    current_url: &str,
    url_change: fn(String) -> ConfigChange,
    key_environment: &str,
    discover: bool,
) -> Result<String, Error> {
    let url = prompt_with_default("API base URL", current_url)?;
    config.change_global(url_change(url.clone()))?;

    let key = std::env::var(key_environment).unwrap_or_default();
    if key.is_empty() {
        println!("{key_environment} is not set. Set it before sending a request.");
    }
    let models = if discover && !key.is_empty() {
        discover_models(&url, &key).unwrap_or_else(|error| {
            println!("Could not list models: {error}");
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let model = choose_model(models)?;
    Ok(format!("{provider}:{model}"))
}

fn configure_local(config: &Config, provider: &str, default_url: &str) -> Result<String, Error> {
    let configured_url = config
        .providers
        .get(provider)
        .map_or(default_url, |configured| configured.base_url.as_str());
    let url = prompt_with_default("API base URL", configured_url)?;
    let authentication = choose_authentication()?;
    config.change_global(ConfigChange::Provider {
        name: provider.to_string(),
        base_url: url.clone(),
        api_key: Some(authentication.config_value().to_string()),
    })?;

    let models = if matches!(authentication, Authentication::Environment { ref value, .. } if value.is_empty())
    {
        println!("The environment variable is not set, so model discovery was skipped.");
        Vec::new()
    } else {
        discover_models(&url, authentication.request_key()).unwrap_or_else(|error| {
            println!("Could not list models: {error}");
            Vec::new()
        })
    };
    let model = choose_model(models)?;
    Ok(format!("{provider}:{model}"))
}

fn choose_model(models: Vec<String>) -> Result<String, Error> {
    if models.is_empty() {
        return nonempty(prompt("Model ID")?, "model ID");
    }
    println!("\nAvailable models:");
    for (index, model) in models.iter().enumerate() {
        println!("  {}. {model}", index + 1);
    }
    let selection = prompt("Model number or exact ID")?;
    if let Ok(number) = selection.parse::<usize>()
        && let Some(model) = number.checked_sub(1).and_then(|index| models.get(index))
    {
        return Ok(model.clone());
    }
    nonempty(selection, "model ID")
}
