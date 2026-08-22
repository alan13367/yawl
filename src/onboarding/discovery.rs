use std::time::Duration;

use serde_json::Value;

use crate::error::Error;

const MAX_DISCOVERED_MODELS: usize = 30;

pub(super) fn discover_models(base_url: &str, api_key: &str) -> Result<Vec<String>, Error> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    fetch_model_ids(&url, AuthStyle::Bearer(api_key))
}

pub(super) fn discover_anthropic_models(
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, Error> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    fetch_model_ids(&url, AuthStyle::Anthropic(api_key))
}

/// How the models request authenticates.
enum AuthStyle<'a> {
    /// `Authorization: Bearer` when a key is present.
    Bearer(&'a str),
    /// `x-api-key` plus the Anthropic version header.
    Anthropic(&'a str),
}

fn fetch_model_ids(url: &str, style: AuthStyle<'_>) -> Result<Vec<String>, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
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
    parse_model_ids(&body)
}

fn parse_model_ids(payload: &str) -> Result<Vec<String>, Error> {
    let payload: Value = serde_json::from_str(payload)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_sorted_deduplicated_and_capped() -> Result<(), Error> {
        let mut data = vec![serde_json::json!({"id": "zeta"})];
        data.extend((0..40).map(|index| serde_json::json!({"id": format!("m{index:02}")})));
        data.push(serde_json::json!({"id": "zeta"}));
        let payload = serde_json::json!({"data": data}).to_string();

        let models = parse_model_ids(&payload)?;

        assert_eq!(models.len(), MAX_DISCOVERED_MODELS);
        assert_eq!(models.first().map(String::as_str), Some("m00"));
        assert_eq!(models.last().map(String::as_str), Some("m29"));
        Ok(())
    }
}
