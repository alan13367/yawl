//! Account-scoped Codex model catalog. The service response is authoritative;
//! a small local snapshot keeps model metadata available between refreshes.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::auth;
use crate::config::{Config, write_json_object};
use crate::error::Error;

const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const MIN_CLIENT_VERSION: &str = "0.155.1";
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
static REFRESH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CodexModel {
    pub slug: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(default)]
    pub supported_in_api: bool,
    pub context_window: Option<u64>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<ReasoningLevel>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReasoningLevel {
    pub effort: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<CodexModel>,
}

#[derive(Deserialize, Serialize)]
struct CachedCatalog {
    account_id: String,
    models: Vec<CodexModel>,
}

fn cache_path(config: &Config) -> PathBuf {
    config.home_dir.join("codex-models.json")
}

fn visible_models(models: Vec<CodexModel>) -> Vec<CodexModel> {
    let mut listed = Vec::new();
    for model in models {
        if model.visibility == "list"
            && model.supported_in_api
            && !model.slug.is_empty()
            && !listed
                .iter()
                .any(|existing: &CodexModel| existing.slug == model.slug)
        {
            listed.push(model);
        }
    }
    listed
}

/// What identifies one version of a file on disk.
#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    state: Option<(u64, u64, std::time::SystemTime)>,
}

impl FileStamp {
    fn of(path: PathBuf) -> Self {
        let state = file_state(&path);
        Self { path, state }
    }
}

/// The file's identity, length, and modification time. The identity catches
/// a file replaced by rename even when the length and time match.
#[cfg(unix)]
fn file_state(path: &Path) -> Option<(u64, u64, std::time::SystemTime)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some((
        metadata.ino(),
        metadata.len(),
        metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
    ))
}

/// The NTFS file index plays the part of the inode. The creation time is not
/// used because file tunneling preserves it across a delete and recreate.
#[cfg(windows)]
fn file_state(path: &Path) -> Option<(u64, u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    let info = crate::paths::file_info(path).ok()?;
    Some((
        info.index,
        metadata.len(),
        metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
    ))
}

/// Model lookups run several times per model step, so the parsed catalog is
/// reused until the login or the catalog file changes on disk.
static LOOKUP: Mutex<Option<(FileStamp, FileStamp, Vec<CodexModel>)>> = Mutex::new(None);

/// Read only the catalog saved for the currently logged-in account. A missing
/// or malformed snapshot simply leaves manual model selection available.
pub(crate) fn cached_models(config: &Config) -> Vec<CodexModel> {
    let auth = FileStamp::of(auth::auth_path(config));
    let catalog = FileStamp::of(cache_path(config));
    let mut lookup = LOOKUP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((cached_auth, cached_catalog, models)) = lookup.as_ref()
        && *cached_auth == auth
        && *cached_catalog == catalog
    {
        return models.clone();
    }
    let models = read_cached_models(config);
    *lookup = Some((auth, catalog, models.clone()));
    models
}

fn read_cached_models(config: &Config) -> Vec<CodexModel> {
    let Ok(Some(credential)) = auth::load_credential(config) else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(cache_path(config)) else {
        return Vec::new();
    };
    if bytes.len() > MAX_CATALOG_BYTES {
        return Vec::new();
    }
    let Ok(cache) = serde_json::from_slice::<CachedCatalog>(&bytes) else {
        return Vec::new();
    };
    if cache.account_id != credential.account_id {
        return Vec::new();
    }
    visible_models(cache.models)
}

fn client_version(config: &Config) -> String {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| config.home_dir.parent().map(|parent| parent.join(".codex")));
    let Some(codex_home) = codex_home else {
        return MIN_CLIENT_VERSION.into();
    };
    let Ok(bytes) = std::fs::read(codex_home.join("models_cache.json")) else {
        return MIN_CLIENT_VERSION.into();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return MIN_CLIENT_VERSION.into();
    };
    let Some(version) = value["client_version"].as_str() else {
        return MIN_CLIENT_VERSION.into();
    };
    let numbers = |value: &str| {
        let parts = value
            .split('.')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        match parts.as_slice() {
            [major, minor, patch] => Some((*major, *minor, *patch)),
            _ => None,
        }
    };
    if numbers(version) > numbers(MIN_CLIENT_VERSION) {
        version.into()
    } else {
        MIN_CLIENT_VERSION.into()
    }
}

/// Fetch the current account's catalog and replace the saved snapshot only
/// after a successful response. Network failures leave the old snapshot intact.
pub(crate) fn refresh_catalog(config: &Config) -> Result<Vec<CodexModel>, Error> {
    // Picker refreshes may overlap when a user closes and reopens /model.
    // Serialize writes because the config writer uses one temporary filename
    // per process.
    let _guard = REFRESH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let credential = auth::load_and_refresh_credential(config)?;
    let version = client_version(config);
    let agent = crate::provider::cancellable_agent(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(6)))
            .build(),
    );
    let mut response = agent
        .get(format!("{MODELS_URL}?client_version={version}"))
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {}", credential.access))
        .header("chatgpt-account-id", &credential.account_id)
        .header("originator", "yawl")
        .call()?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_CATALOG_BYTES as u64)
        .read_to_string()?;
    if status != 200 {
        return Err(Error::Http { status, body });
    }
    let payload: ModelsResponse = serde_json::from_str(&body)?;
    let models = visible_models(payload.models);
    let cache = CachedCatalog {
        account_id: credential.account_id,
        models: models.clone(),
    };
    let Value::Object(object) = serde_json::to_value(cache)? else {
        return Err(Error::Protocol("invalid Codex model catalog".into()));
    };
    write_json_object(&cache_path(config), &object)?;
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_visible_api_models_are_listed() -> Result<(), Error> {
        let payload: ModelsResponse = serde_json::from_str(
            r#"{"models":[{"slug":"gpt-6-sol","display_name":"GPT-6 Sol","visibility":"list","supported_in_api":true},{"slug":"gpt-6-sol","visibility":"list","supported_in_api":true},{"slug":"internal","visibility":"hide","supported_in_api":true},{"slug":"unavailable","visibility":"list","supported_in_api":false}]}"#,
        )?;
        let models = visible_models(payload.models);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].slug, "gpt-6-sol");
        Ok(())
    }
}
