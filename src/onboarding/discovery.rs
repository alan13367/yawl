use std::time::Duration;

use serde_json::Value;

use crate::error::Error;

const MAX_DISCOVERED_MODELS: usize = 30;

pub(super) fn discover_models(base_url: &str, api_key: &str) -> Result<Vec<String>, Error> {
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
