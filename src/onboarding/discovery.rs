use std::time::Duration;

use serde_json::Value;

use crate::error::Error;

const MAX_DISCOVERED_MODELS: usize = 30;

/// A model listed by a provider's models endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredModel {
    pub(crate) id: String,
    /// The served context limit, when the server reports one. oMLX and vLLM
    /// add a non-standard `max_model_len` to each OpenAI-style entry.
    pub(crate) context_window: Option<u64>,
}

impl DiscoveredModel {
    /// A model with no reported metadata.
    pub(crate) fn named(id: String) -> Self {
        Self {
            id,
            context_window: None,
        }
    }
}

pub(super) fn discover_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, Error> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    fetch_models(&url, AuthStyle::Bearer(api_key))
}

pub(super) fn discover_anthropic_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<DiscoveredModel>, Error> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    fetch_models(&url, AuthStyle::Anthropic(api_key))
}

/// How the models request authenticates.
enum AuthStyle<'a> {
    /// `Authorization: Bearer` when a key is present.
    Bearer(&'a str),
    /// `x-api-key` plus the Anthropic version header.
    Anthropic(&'a str),
}

fn fetch_models(url: &str, style: AuthStyle<'_>) -> Result<Vec<DiscoveredModel>, Error> {
    let agent = crate::provider::cancellable_agent(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build(),
    );
    let mut request = agent.get(url).header("accept", "application/json");
    request = match style {
        AuthStyle::Bearer(key) if !key.is_empty() => {
            request.header("authorization", format!("Bearer {key}"))
        }
        AuthStyle::Bearer(_) => request,
        AuthStyle::Anthropic(key) => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
    };
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
    parse_models(&body)
}

fn parse_models(payload: &str) -> Result<Vec<DiscoveredModel>, Error> {
    let payload: Value = serde_json::from_str(payload)?;
    let mut models = payload["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            Some(DiscoveredModel {
                id: model["id"].as_str()?.to_string(),
                context_window: model["max_model_len"].as_u64().filter(|window| *window > 0),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    models.truncate(MAX_DISCOVERED_MODELS);
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_sorted_deduplicated_and_capped() -> Result<(), Error> {
        let mut data = vec![serde_json::json!({"id": "zeta"})];
        data.extend((0..40).map(|index| serde_json::json!({"id": format!("m{index:02}")})));
        data.push(serde_json::json!({"id": "zeta"}));
        let payload = serde_json::json!({"data": data}).to_string();

        let models = parse_models(&payload)?;

        assert_eq!(models.len(), MAX_DISCOVERED_MODELS);
        assert_eq!(models.first().map(|model| model.id.as_str()), Some("m00"));
        assert_eq!(models.last().map(|model| model.id.as_str()), Some("m29"));
        Ok(())
    }

    #[test]
    fn served_context_length_is_read_when_reported() -> Result<(), Error> {
        let payload = serde_json::json!({"data": [
            {"id": "omlx-model", "object": "model", "owned_by": "omlx", "max_model_len": 65536},
            {"id": "plain-model", "object": "model"},
            {"id": "zero-model", "max_model_len": 0},
            {"id": "text-model", "max_model_len": "big"}
        ]})
        .to_string();

        let models = parse_models(&payload)?;

        let window = |id: &str| {
            models
                .iter()
                .find(|model| model.id == id)
                .and_then(|model| model.context_window)
        };
        assert_eq!(window("omlx-model"), Some(65_536));
        assert_eq!(window("plain-model"), None);
        assert_eq!(window("zero-model"), None);
        assert_eq!(window("text-model"), None);
        Ok(())
    }
}
