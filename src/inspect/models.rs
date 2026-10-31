//! `yawl --list-models`: every provider Yawl can reach, its credential
//! state, and the models it offers with their capabilities. `--refresh`
//! asks each provider for its current models instead of trusting what was
//! saved.

use serde_json::{Value, json};

use crate::config::Config;
use crate::error::Error;
use crate::onboarding::provider::{DiscoveredModel, ProviderId};

/// One provider and what is known about reaching it.
#[derive(Debug)]
pub(super) struct ProviderStatus {
    /// The prefix model ids use, such as `openai-codex` or `ollama`.
    pub(super) name: String,
    pub(super) id: ProviderId,
    pub(super) kind: &'static str,
    pub(super) base_url: Option<String>,
    /// `logged_in`, `present`, `missing`, or `none` for a keyless
    /// OpenAI-compatible server.
    pub(super) credential: &'static str,
    /// Whether the user set this provider up, rather than it being a
    /// built-in or a default local entry.
    pub(super) configured: bool,
    /// Set by `--refresh`: models the provider listed, or why listing failed.
    pub(super) probe: Option<Result<Vec<DiscoveredModel>, String>>,
}

impl ProviderStatus {
    /// Whether a request could carry the credential this provider needs.
    pub(super) fn has_credential(&self) -> bool {
        self.credential != "missing"
    }
}

/// Every provider in the setup catalog, with its credential state.
pub(super) fn providers(config: &Config) -> Vec<ProviderStatus> {
    crate::onboarding::provider::provider_catalog(config)
        .into_iter()
        .filter_map(|definition| {
            let (name, kind, base_url) = match &definition.id {
                ProviderId::Codex => ("openai-codex".to_string(), "codex", None),
                ProviderId::Anthropic => (
                    "anthropic".to_string(),
                    "anthropic",
                    Some(crate::onboarding::provider::default_endpoint(
                        &definition.id,
                        config,
                    )),
                ),
                ProviderId::OpenAi => (
                    "openai".to_string(),
                    "openai",
                    Some(crate::onboarding::provider::default_endpoint(
                        &definition.id,
                        config,
                    )),
                ),
                ProviderId::Compatible(name) => (
                    name.clone(),
                    "openai-compatible",
                    config
                        .providers
                        .get(name)
                        .map(|provider| provider.base_url.clone()),
                ),
                ProviderId::Other => return None,
            };
            Some(ProviderStatus {
                credential: credential(config, &definition.id),
                name,
                kind,
                base_url,
                configured: definition.configured,
                id: definition.id,
                probe: None,
            })
        })
        .collect()
}

fn credential(config: &Config, id: &ProviderId) -> &'static str {
    match id {
        ProviderId::Codex => {
            if crate::onboarding::provider::credential_is_configured(config, id) {
                "logged_in"
            } else {
                "missing"
            }
        }
        ProviderId::Anthropic | ProviderId::OpenAi => {
            if crate::onboarding::provider::credential_is_configured(config, id) {
                "present"
            } else {
                "missing"
            }
        }
        ProviderId::Compatible(_) => {
            let environment = crate::onboarding::provider::credential_environment_name(id)
                .and_then(|name| std::env::var(name).ok())
                .is_some_and(|value| !value.trim().is_empty());
            if environment || crate::onboarding::provider::credential_is_configured(config, id) {
                "present"
            } else {
                "none"
            }
        }
        ProviderId::Other => "missing",
    }
}

/// Lists each provider's models over the network, in parallel. Providers
/// that need a missing credential are skipped. Codex refreshes its saved
/// catalog, so later lookups see the new capabilities too.
pub(super) fn probe(config: &Config, providers: &mut [ProviderStatus]) {
    std::thread::scope(|scope| {
        let handles = providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| provider.has_credential())
            .map(|(index, provider)| {
                let id = provider.id.clone();
                let base_url = provider.base_url.clone().unwrap_or_default();
                (
                    index,
                    scope.spawn(move || list_provider_models(config, &id, &base_url)),
                )
            })
            .collect::<Vec<_>>();
        for (index, handle) in handles {
            let result = handle
                .join()
                .unwrap_or_else(|_| Err(Error::Protocol("model listing panicked".into())));
            providers[index].probe = Some(result.map_err(|error| error.to_string()));
        }
    });
}

pub(super) fn list_provider_models(
    config: &Config,
    id: &ProviderId,
    base_url: &str,
) -> Result<Vec<DiscoveredModel>, Error> {
    if *id == ProviderId::Codex {
        return Ok(crate::provider::codex::refresh_catalog(config)?
            .into_iter()
            .map(|model| DiscoveredModel {
                id: model.slug,
                context_window: model.context_window,
            })
            .collect());
    }
    let key = crate::onboarding::provider::request_credential(
        config,
        id,
        &crate::onboarding::provider::CredentialChoice::Keep,
    );
    crate::onboarding::provider::discover_models(id, base_url, &key)
}

/// One model a caller can pass to `--model`.
#[derive(Debug, PartialEq)]
pub(super) struct ModelInfo {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) provider: String,
    /// `saved` (configured or from the saved Codex catalog), `discovered`
    /// (listed by `--refresh` only), or `default` (the configured model,
    /// otherwise unlisted).
    pub(super) source: &'static str,
    /// The context limit the provider reported while listing, which beats
    /// Yawl's fallback for models it has no saved limit for.
    pub(super) listed_context_window: Option<u64>,
}

impl ModelInfo {
    fn context_window(&self, config: &Config) -> u64 {
        match self.listed_context_window {
            Some(window) if self.source == "discovered" => window,
            _ => crate::model::context_window(config, &self.id),
        }
    }
}

/// Saved models first, then models only a probe listed, then the default
/// model if nothing else named it.
pub(super) fn models(
    config: &Config,
    providers: &[ProviderStatus],
    default_model: Option<&str>,
) -> Vec<ModelInfo> {
    let mut models = crate::model::available_models(config)
        .into_iter()
        .map(|(id, name)| ModelInfo {
            provider: provider_name(&id),
            id,
            name,
            source: "saved",
            listed_context_window: None,
        })
        .collect::<Vec<_>>();
    for provider in providers {
        let Some(Ok(listed)) = &provider.probe else {
            continue;
        };
        for model in listed {
            let id = format!("{}:{}", provider.name, model.id);
            if !models.iter().any(|known| known.id == id) {
                models.push(ModelInfo {
                    id,
                    name: model.id.clone(),
                    provider: provider.name.clone(),
                    source: "discovered",
                    listed_context_window: model.context_window,
                });
            }
        }
    }
    if let Some(default_model) = default_model
        && !models.iter().any(|known| known.id == default_model)
    {
        models.push(ModelInfo {
            id: default_model.to_string(),
            name: default_model.to_string(),
            provider: provider_name(default_model),
            source: "default",
            listed_context_window: None,
        });
    }
    models
}

/// The provider a model spec routes to, by the name its prefix uses.
pub(super) fn provider_name(spec: &str) -> String {
    match spec.split_once(':') {
        Some((prefix, _)) => prefix.to_string(),
        None if spec.starts_with("claude") => "anthropic".into(),
        None => "openai".into(),
    }
}

/// What `--model` accepts for `spec`, as reported to callers.
pub(super) fn capabilities(config: &Config, spec: &str) -> Value {
    json!({
        "context_window": crate::model::context_window(config, spec),
        "max_output_tokens": crate::model::max_tokens(config, spec),
        "reasoning_efforts": crate::model::reasoning_efforts(config, spec),
        "images": crate::model::supports_images(config, spec),
    })
}

pub(super) fn provider_json(provider: &ProviderStatus) -> Value {
    let mut value = json!({
        "name": provider.name,
        "kind": provider.kind,
        "base_url": provider.base_url,
        "credential": provider.credential,
        "configured": provider.configured,
    });
    if let Some(probe) = &provider.probe {
        value["reachable"] = Value::from(probe.is_ok());
        match probe {
            Ok(models) => value["listed_models"] = Value::from(models.len()),
            Err(error) => value["error"] = Value::from(error.as_str()),
        }
    }
    value
}

/// Prints the listing. The JSON form carries everything; the text form is
/// a compact summary for people.
pub(crate) fn run(config: &Config, machine: bool, refresh: bool) -> i32 {
    let mut providers = providers(config);
    if refresh {
        probe(config, &mut providers);
    }
    let default_model = config.model.as_deref();
    let models = models(config, &providers, default_model);
    let mut notes = Vec::new();
    if !refresh {
        notes.push(
            "Only saved models are listed. Add --refresh to ask each provider for its current \
             models; any model id the provider serves also works with --model.",
        );
    }
    if machine {
        let value = json!({
            "default_model": default_model,
            "subagent_model": config.subagent_model,
            "reasoning_effort": config.reasoning_effort,
            "providers": providers.iter().map(provider_json).collect::<Vec<_>>(),
            "models": models.iter().map(|model| {
                let mut entry = json!({
                    "id": model.id,
                    "name": model.name,
                    "provider": model.provider,
                    "source": model.source,
                    "default": Some(model.id.as_str()) == default_model,
                });
                if let (Value::Object(entry), Value::Object(capabilities)) =
                    (&mut entry, capabilities(config, &model.id))
                {
                    entry.extend(capabilities);
                }
                entry["context_window"] = Value::from(model.context_window(config));
                entry
            }).collect::<Vec<_>>(),
            "notes": notes,
        });
        println!("{}", crate::text::json_line(&value));
        return 0;
    }
    print_text(config, &providers, &models, default_model, &notes);
    0
}

fn print_text(
    config: &Config,
    providers: &[ProviderStatus],
    models: &[ModelInfo],
    default_model: Option<&str>,
    notes: &[&str],
) {
    let safe = |text: &str| crate::text::sanitize_line(text, crate::text::Tabs::Space);
    println!("Default model: {}", safe(default_model.unwrap_or("(none)")));
    println!("\nProviders:");
    println!("  {:<16} {:<10} endpoint", "name", "credential");
    // Default local entries are noise until the user sets one up or a
    // probe finds its server running.
    let shown = providers.iter().filter(|provider| {
        provider.configured
            || provider.kind != "openai-compatible"
            || matches!(provider.probe, Some(Ok(_)))
    });
    for provider in shown {
        let reach = match &provider.probe {
            None => String::new(),
            Some(Ok(listed)) => format!("  reachable, {} models", listed.len()),
            Some(Err(error)) => format!(
                "  unreachable: {}",
                safe(&crate::text::ellipsize(error, 80))
            ),
        };
        println!(
            "  {:<16} {:<10} {}{reach}",
            safe(&provider.name),
            provider.credential,
            safe(provider.base_url.as_deref().unwrap_or("ChatGPT login")),
        );
    }
    println!("\nModels:");
    if models.is_empty() {
        println!("  (none saved)");
    }
    for model in models {
        let marker = if Some(model.id.as_str()) == default_model {
            '*'
        } else {
            ' '
        };
        let efforts = crate::model::reasoning_efforts(config, &model.id);
        let mut details = vec![format!("{}k context", model.context_window(config) / 1000)];
        if !efforts.is_empty() {
            details.push(format!("reasoning {}", efforts.join("/")));
        }
        if crate::model::supports_images(config, &model.id) {
            details.push("images".into());
        }
        if model.source != "saved" {
            details.push(model.source.into());
        }
        println!("{marker} {}  {}", safe(&model.id), details.join(", "));
    }
    for note in notes {
        println!("\n{note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str, probe: Option<Result<Vec<DiscoveredModel>, String>>) -> ProviderStatus {
        ProviderStatus {
            name: name.into(),
            id: ProviderId::Compatible(name.into()),
            kind: "openai-compatible",
            base_url: Some("http://localhost:1/v1".into()),
            credential: "none",
            configured: true,
            probe,
        }
    }

    #[test]
    fn provider_names_follow_model_routing() {
        assert_eq!(provider_name("ollama:llama3.1:8b"), "ollama");
        assert_eq!(provider_name("claude-sonnet-4-5"), "anthropic");
        assert_eq!(provider_name("gpt-4o"), "openai");
    }

    #[test]
    fn probes_add_unsaved_models_and_the_default_is_always_listed() {
        let config = Config::test_default();
        let providers = [
            status(
                "ollama",
                Some(Ok(vec![DiscoveredModel::named("qwen3:8b".into())])),
            ),
            status("lmstudio", Some(Err("connection refused".into()))),
        ];
        let models = models(&config, &providers, Some("anthropic:claude-x"));
        let ids = models
            .iter()
            .map(|model| (model.id.as_str(), model.source))
            .collect::<Vec<_>>();
        assert!(ids.contains(&("ollama:qwen3:8b", "discovered")));
        assert!(ids.contains(&("anthropic:claude-x", "default")));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn provider_json_reports_probe_results() {
        let ok = provider_json(&status("ollama", Some(Ok(Vec::new()))));
        assert_eq!(ok["reachable"], true);
        assert_eq!(ok["listed_models"], 0);
        let failed = provider_json(&status("lmstudio", Some(Err("refused".into()))));
        assert_eq!(failed["reachable"], false);
        assert_eq!(failed["error"], "refused");
        let unprobed = provider_json(&status("vllm", None));
        assert!(unprobed.get("reachable").is_none());
    }
}
