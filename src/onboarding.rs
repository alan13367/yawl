//! First-run provider and model setup.

use std::io::{self, Write};
use std::time::Duration;

use serde_json::Value;

use crate::config::{Config, ConfigChange};
use crate::error::Error;

const MAX_DISCOVERED_MODELS: usize = 30;

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

enum Authentication {
    None,
    Environment { reference: String, value: String },
    Literal(String),
}

impl Authentication {
    fn config_value(&self) -> &str {
        match self {
            Authentication::None => "-",
            Authentication::Environment { reference, .. } => reference,
            Authentication::Literal(value) => value,
        }
    }

    fn request_key(&self) -> &str {
        match self {
            Authentication::None => "",
            Authentication::Environment { value, .. } | Authentication::Literal(value) => value,
        }
    }
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

fn choose_authentication() -> Result<Authentication, Error> {
    println!(
        "\nAuthentication:\n  1. No API key\n  2. Read the key from an environment variable\n  3. Enter an API key now"
    );
    match prompt("Authentication number")?.as_str() {
        "1" => Ok(Authentication::None),
        "2" => {
            let name = prompt("Environment variable name, without '$'")?;
            validate_environment_name(&name)?;
            let value = std::env::var(&name).unwrap_or_default();
            Ok(Authentication::Environment {
                reference: format!("${name}"),
                value,
            })
        }
        "3" => {
            let key = prompt_secret("API key")?;
            if key.is_empty() {
                Err(Error::Config("API key must not be empty".into()))
            } else {
                Ok(Authentication::Literal(key))
            }
        }
        _ => Err(Error::Config(
            "authentication must be a number from 1 through 3".into(),
        )),
    }
}

fn discover_models(base_url: &str, api_key: &str) -> Result<Vec<String>, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = agent.get(&url).header("accept", "application/json");
    if !api_key.is_empty() {
        request = request.header("authorization", format!("Bearer {api_key}"));
    }
    let mut response = request.call()?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_string()?;
    if status != 200 {
        return Err(Error::Http { status, body });
    }
    let payload: Value = serde_json::from_str(&body)?;
    let mut models = payload["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| model["id"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();
    models.truncate(MAX_DISCOVERED_MODELS);
    Ok(models)
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

fn prompt(label: &str) -> Result<String, Error> {
    print!("{label}: ");
    io::stdout().flush()?;
    read_line()
}

fn prompt_with_default(label: &str, default: &str) -> Result<String, Error> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let value = read_line()?;
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

fn prompt_secret(label: &str) -> Result<String, Error> {
    print!("{label}, input hidden: ");
    io::stdout().flush()?;

    // SAFETY: `termios` is initialized by `tcgetattr` before use. stdin is a
    // TTY during onboarding, and the guard restores the original flags.
    let original = unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        original
    };
    let guard = EchoGuard(original);
    let value = read_line();
    drop(guard);
    println!();
    value
}

struct EchoGuard(libc::termios);

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: The value came from a successful `tcgetattr` call for
        // stdin and remains initialized until this guard is dropped.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

fn read_line() -> Result<String, Error> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err(Error::Config("onboarding canceled".into()));
    }
    Ok(line.trim().to_string())
}

fn nonempty(value: String, name: &str) -> Result<String, Error> {
    if value.is_empty() {
        Err(Error::Config(format!("{name} must not be empty")))
    } else {
        Ok(value)
    }
}

fn validate_environment_name(name: &str) -> Result<(), Error> {
    let mut bytes = name.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if valid_start && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        Ok(())
    } else {
        Err(Error::Config("invalid environment variable name".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_environment_variable_names() {
        assert!(validate_environment_name("OMLX_API_KEY").is_ok());
        assert!(validate_environment_name("2BAD").is_err());
        assert!(validate_environment_name("BAD-NAME").is_err());
    }
}
