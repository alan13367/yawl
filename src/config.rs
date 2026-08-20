use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::Error;

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.85;
const OPENAI_COMPLETIONS_API: &str = "openai-completions";

/// OpenAI Chat Completions compatibility switches understood by Yawl.
///
/// The camelCase aliases let a provider block be copied from pi's
/// `models.json`. Unsupported pi compatibility fields are ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct OpenAiCompatibility {
    #[serde(alias = "supportsUsageInStreaming")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(alias = "supportsFinishReason")]
    pub supports_finish_reason: Option<bool>,
    #[serde(alias = "requiresToolResultName")]
    pub requires_tool_result_name: Option<bool>,
    #[serde(alias = "requiresReasoningContentOnAssistantMessages")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
    #[serde(alias = "maxTokensField")]
    pub max_tokens_field: Option<String>,
}

impl OpenAiCompatibility {
    pub(crate) fn apply(&mut self, other: OpenAiCompatibility) {
        if other.supports_usage_in_streaming.is_some() {
            self.supports_usage_in_streaming = other.supports_usage_in_streaming;
        }
        if other.supports_finish_reason.is_some() {
            self.supports_finish_reason = other.supports_finish_reason;
        }
        if other.requires_tool_result_name.is_some() {
            self.requires_tool_result_name = other.requires_tool_result_name;
        }
        if other
            .requires_reasoning_content_on_assistant_messages
            .is_some()
        {
            self.requires_reasoning_content_on_assistant_messages =
                other.requires_reasoning_content_on_assistant_messages;
        }
        if other.max_tokens_field.is_some() {
            self.max_tokens_field = other.max_tokens_field;
        }
    }

    pub fn usage_in_stream(&self) -> bool {
        self.supports_usage_in_streaming.unwrap_or(true)
    }

    pub fn finish_reason_in_stream(&self) -> bool {
        self.supports_finish_reason.unwrap_or(true)
    }

    pub fn tool_result_name_required(&self) -> bool {
        self.requires_tool_result_name.unwrap_or(false)
    }

    pub fn reasoning_content_on_assistant_messages(&self) -> bool {
        self.requires_reasoning_content_on_assistant_messages
            .unwrap_or(false)
    }

    pub fn max_tokens_field(&self) -> &str {
        self.max_tokens_field.as_deref().unwrap_or("max_tokens")
    }
}

/// Optional metadata for a model exposed by a custom provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "contextWindow")]
    pub context_window: Option<u64>,
    #[serde(default, alias = "maxTokens")]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub compat: OpenAiCompatibility,
}

/// Effective configuration for one OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api: String,
    pub api_key: Option<String>,
    pub auth_header: Option<bool>,
    pub headers: HashMap<String, String>,
    pub models: Vec<ModelConfig>,
    pub compat: OpenAiCompatibility,
}

impl ProviderConfig {
    fn openai_compatible(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            api: OPENAI_COMPLETIONS_API.to_string(),
            api_key: None,
            auth_header: None,
            headers: HashMap::new(),
            models: Vec::new(),
            compat: OpenAiCompatibility::default(),
        }
    }

    fn apply(&mut self, file: ProviderFile) {
        if let Some(value) = file.base_url {
            self.base_url = value;
        }
        if let Some(value) = file.api {
            self.api = value;
        }
        if let Some(value) = file.api_key {
            self.api_key = Some(value);
        }
        if let Some(value) = file.auth_header {
            self.auth_header = Some(value);
        }
        if let Some(headers) = file.headers {
            self.headers.extend(headers);
        }
        if let Some(models) = file.models {
            self.models = models;
        }
        if let Some(compat) = file.compat {
            self.compat.apply(compat);
        }
    }
}

/// On-disk shape of `config.json`. All fields are optional so the project
/// file can override only the keys it cares about.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    model: Option<String>,
    anthropic_base_url: Option<String>,
    openai_base_url: Option<String>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    /// Whether reasoning content is omitted from terminal output.
    hide_reasoning: Option<bool>,
    /// Per-model context window overrides, e.g. {"my-local-model": 32768}.
    context_windows: Option<HashMap<String, u64>>,
    /// Whether automatic context compaction is enabled.
    auto_compact: Option<bool>,
    /// Fraction of the context window at which auto-compaction triggers.
    compact_threshold: Option<f64>,
    skill_dirs: Option<Vec<String>>,
    providers: Option<HashMap<String, ProviderFile>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProviderFile {
    #[serde(alias = "baseUrl")]
    base_url: Option<String>,
    api: Option<String>,
    #[serde(alias = "apiKey")]
    api_key: Option<String>,
    #[serde(alias = "authHeader")]
    auth_header: Option<bool>,
    headers: Option<HashMap<String, String>>,
    models: Option<Vec<ModelConfig>>,
    compat: Option<OpenAiCompatibility>,
}

/// Effective configuration: defaults <- `~/.yawl/config.json` <-
/// `./.yawl/config.json`.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: Option<String>,
    pub anthropic_base_url: String,
    pub openai_base_url: String,
    pub max_tokens: u32,
    /// Reasoning effort sent to OpenAI Codex (`minimal` through `max`).
    /// `None` leaves the provider default unchanged.
    pub reasoning_effort: Option<String>,
    pub hide_reasoning: bool,
    pub context_windows: HashMap<String, u64>,
    pub auto_compact: bool,
    pub compact_threshold: f64,
    /// Directories containing `NAME/SKILL.md` or `NAME.md` skills.
    pub skill_dirs: Vec<PathBuf>,
    pub providers: HashMap<String, ProviderConfig>,
    /// `~/.yawl`.
    pub home_dir: PathBuf,
    /// `./.yawl`.
    pub project_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Config, Error> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Error::Config("HOME is not set".into()))?;
        let home_dir = home.join(".yawl");
        let project_dir = PathBuf::from(".yawl");

        let mut cfg = Config {
            model: None,
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            openai_base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            skill_dirs: vec![home.join(".yawl/skills"), home.join(".agents/skills")],
            providers: default_local_providers(),
            home_dir,
            project_dir,
        };
        let global = cfg.global_config_path();
        let project = cfg.project_dir.join("config.json");
        for path in [global, project] {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    let file: ConfigFile = serde_json::from_str(&text)
                        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
                    cfg.apply(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::Config(format!("{}: {e}", path.display()))),
            }
        }
        Ok(cfg)
    }

    fn apply(&mut self, file: ConfigFile) {
        if let Some(value) = file.model {
            self.model = (!value.trim().is_empty()).then_some(value);
        }
        if let Some(value) = file.anthropic_base_url {
            self.anthropic_base_url = value;
        }
        if let Some(value) = file.openai_base_url {
            self.openai_base_url = value;
        }
        if let Some(value) = file.max_tokens {
            self.max_tokens = value.max(1);
        }
        if let Some(value) = file.reasoning_effort {
            self.reasoning_effort = normalize_reasoning_effort(&value).map(str::to_string);
        }
        if let Some(value) = file.hide_reasoning {
            self.hide_reasoning = value;
        }
        if let Some(map) = file.context_windows {
            self.context_windows.extend(map);
        }
        if let Some(value) = file.auto_compact {
            self.auto_compact = value;
        }
        if let Some(value) = file.compact_threshold {
            self.compact_threshold = value.clamp(0.1, 0.99);
        }
        if let Some(dirs) = file.skill_dirs {
            self.skill_dirs = dirs
                .into_iter()
                .filter(|dir| !dir.trim().is_empty())
                .map(|dir| expand_home_path(&dir, &self.home_dir))
                .collect();
        }
        if let Some(providers) = file.providers {
            for (name, provider) in providers {
                self.providers
                    .entry(name)
                    .or_insert_with(|| ProviderConfig::openai_compatible(""))
                    .apply(provider);
            }
        }
    }

    pub fn global_config_path(&self) -> PathBuf {
        self.home_dir.join("config.json")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home_dir.join("sessions")
    }

    /// Tool scan order. Later directories override earlier ones on name
    /// collisions, so project tools win over global tools.
    pub fn tool_dirs(&self) -> [PathBuf; 2] {
        [self.home_dir.join("tools"), self.project_dir.join("tools")]
    }

    /// Returns a configured custom provider and the model ID after its
    /// `provider:` prefix. Model IDs may themselves contain colons.
    pub fn custom_provider_for<'cfg, 'model>(
        &'cfg self,
        model_spec: &'model str,
    ) -> Option<(&'model str, &'cfg ProviderConfig, &'model str)> {
        let (name, model) = model_spec.split_once(':')?;
        self.providers
            .get(name)
            .map(|provider| (name, provider, model))
    }

    pub fn configured_model(&self, model_spec: &str) -> Option<&ModelConfig> {
        let (_, provider, model) = self.custom_provider_for(model_spec)?;
        provider
            .models
            .iter()
            .find(|candidate| candidate.id == model)
    }

    /// Context window for a model: explicit override, custom-provider model
    /// metadata, then a name-based heuristic.
    pub fn context_window_for(&self, model: &str) -> u64 {
        let bare = model
            .strip_prefix("anthropic:")
            .or_else(|| model.strip_prefix("openai:"))
            .or_else(|| model.strip_prefix("openai-codex:"))
            .or_else(|| self.custom_provider_for(model).map(|(_, _, model)| model))
            .unwrap_or(model);
        if let Some(&window) = self
            .context_windows
            .get(model)
            .or_else(|| self.context_windows.get(bare))
        {
            return window;
        }
        if let Some(window) = self
            .configured_model(model)
            .and_then(|configured| configured.context_window)
        {
            return window;
        }
        if model.starts_with("openai-codex:")
            && let Some((_, _, window)) = crate::provider::codex::MODELS
                .iter()
                .find(|(id, _, _)| *id == bare)
        {
            return *window;
        }
        if bare.starts_with("claude") {
            200_000
        } else {
            128_000
        }
    }

    /// Applies a model's output limit without allowing it to raise the
    /// user-configured global limit.
    pub fn max_tokens_for(&self, model: &str) -> u32 {
        self.configured_model(model)
            .and_then(|configured| configured.max_tokens)
            .filter(|limit| *limit > 0)
            .map_or(self.max_tokens, |limit| self.max_tokens.min(limit))
    }

    /// Configured custom models as `(provider:model, display name)` pairs.
    pub fn available_models(&self) -> Vec<(String, String)> {
        let mut models = self
            .providers
            .iter()
            .flat_map(|(provider_name, provider)| {
                provider.models.iter().map(move |model| {
                    (
                        format!("{provider_name}:{}", model.id),
                        model.name.clone().unwrap_or_else(|| model.id.clone()),
                    )
                })
            })
            .collect::<Vec<_>>();
        models.extend(
            crate::provider::codex::MODELS
                .iter()
                .map(|(id, name, _)| (format!("openai-codex:{id}"), (*name).to_string())),
        );
        models.sort_by(|left, right| left.0.cmp(&right.0));
        models
    }

    /// Saves a scalar setting to the global config while preserving all
    /// unrelated JSON fields.
    pub fn save_global_setting(&self, key: &str, value: Value) -> Result<(), Error> {
        if !matches!(
            key,
            "model"
                | "anthropic_base_url"
                | "openai_base_url"
                | "max_tokens"
                | "reasoning_effort"
                | "hide_reasoning"
                | "auto_compact"
                | "compact_threshold"
        ) {
            return Err(Error::Config(format!("unsupported setting '{key}'")));
        }
        self.update_global_json(|root| {
            root.insert(key.to_string(), value);
            Ok(())
        })
    }

    /// Saves the complete skill search path to the global config.
    pub fn save_global_skill_dirs(&self, dirs: &[PathBuf]) -> Result<(), Error> {
        let home = self.home_dir.parent();
        let values = dirs
            .iter()
            .map(|dir| {
                home.and_then(|home| dir.strip_prefix(home).ok())
                    .map_or_else(
                        || dir.display().to_string(),
                        |relative| format!("~/{}", relative.display()),
                    )
            })
            .collect::<Vec<_>>();
        self.update_global_json(|root| {
            root.insert("skill_dirs".into(), json!(values));
            Ok(())
        })
    }

    /// Saves a context-window override for one model to the global config.
    pub fn save_global_context_window(&self, model: &str, window: u64) -> Result<(), Error> {
        self.update_global_json(|root| {
            let windows = object_field(root, "context_windows")?;
            windows.insert(model.to_string(), json!(window));
            Ok(())
        })
    }

    /// Adds or updates an OpenAI-compatible provider in the global config.
    /// Omitting `api_key` preserves an existing key. `"-"` removes it.
    pub fn save_global_provider(
        &self,
        name: &str,
        base_url: &str,
        api_key: Option<&str>,
    ) -> Result<(), Error> {
        validate_provider_name(name)?;
        if matches!(name, "anthropic" | "openai") {
            return Err(Error::Config(format!(
                "'{name}' is built in; use anthropic_base_url or openai_base_url"
            )));
        }
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            return Err(Error::Config(
                "provider URL must start with http:// or https://".into(),
            ));
        }
        self.update_global_json(|root| {
            let providers = object_field(root, "providers")?;
            let provider = providers
                .entry(name.to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(provider) = provider else {
                return Err(Error::Config(format!(
                    "providers.{name} must be a JSON object"
                )));
            };
            provider.insert("base_url".into(), json!(base_url));
            provider.insert("api".into(), json!(OPENAI_COMPLETIONS_API));
            provider.remove("baseUrl");
            if let Some(api_key) = api_key {
                provider.remove("apiKey");
                if api_key == "-" {
                    provider.remove("api_key");
                    provider.remove("auth_header");
                    provider.remove("authHeader");
                } else {
                    provider.insert("api_key".into(), json!(api_key));
                    provider.insert("auth_header".into(), json!(true));
                }
            }
            Ok(())
        })
    }

    fn update_global_json(
        &self,
        update: impl FnOnce(&mut Map<String, Value>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let path = self.global_config_path();
        let mut root = read_json_object(&path)?;
        update(&mut root)?;
        write_json_object(&path, &root)
    }
}

fn expand_home_path(value: &str, yawl_home: &Path) -> PathBuf {
    if value == "~" {
        return yawl_home.parent().unwrap_or(yawl_home).to_path_buf();
    }
    if let Some(relative) = value.strip_prefix("~/") {
        return yawl_home.parent().unwrap_or(yawl_home).join(relative);
    }
    PathBuf::from(value)
}

fn default_local_providers() -> HashMap<String, ProviderConfig> {
    let mut providers = [
        ("ollama", "http://127.0.0.1:11434/v1"),
        ("lmstudio", "http://127.0.0.1:1234/v1"),
        ("omlx", "http://127.0.0.1:8000/v1"),
    ]
    .into_iter()
    .map(|(name, url)| (name.to_string(), ProviderConfig::openai_compatible(url)))
    .collect::<HashMap<_, _>>();
    if let Some(omlx) = providers.get_mut("omlx") {
        omlx.compat.requires_reasoning_content_on_assistant_messages = Some(true);
    }
    providers
}

/// Valid OpenAI Codex reasoning efforts. `default` and `off` omit the
/// request field and let the service choose its default behavior.
pub fn normalize_reasoning_effort(value: &str) -> Option<&str> {
    match value {
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => Some(value),
        "default" | "off" => None,
        _ => None,
    }
}

fn validate_provider_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Config(
            "provider name may contain only letters, numbers, '-' and '_'".into(),
        ));
    }
    Ok(())
}

fn object_field<'a>(
    root: &'a mut Map<String, Value>,
    name: &str,
) -> Result<&'a mut Map<String, Value>, Error> {
    let value = root
        .entry(name.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match value {
        Value::Object(object) => Ok(object),
        _ => Err(Error::Config(format!("'{name}' must be a JSON object"))),
    }
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text)
            .map_err(|error| Error::Config(format!("{}: {error}", path.display())))?
        {
            Value::Object(object) => Ok(object),
            _ => Err(Error::Config(format!(
                "{}: top-level JSON value must be an object",
                path.display()
            ))),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(Error::Config(format!("{}: {error}", path.display()))),
    }
}

fn write_json_object(path: &Path, root: &Map<String, Value>) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        serde_json::to_writer_pretty(&mut file, &Value::Object(root.clone()))?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok::<(), Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Resolves pi-style `$ENV_VAR` and `${ENV_VAR}` references in provider
/// keys and header values. `$$` emits `$` and `$!` emits `!`.
pub(crate) fn resolve_config_value(value: &str) -> Result<String, Error> {
    if value.starts_with('!') {
        return Err(Error::Config(
            "provider values beginning with '!' are not supported; use an environment variable"
                .into(),
        ));
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len());
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] != '$' {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        let Some(next) = chars.get(index + 1).copied() else {
            output.push('$');
            break;
        };
        if matches!(next, '$' | '!') {
            output.push(if next == '$' { '$' } else { '!' });
            index += 2;
            continue;
        }
        let (name, next_index) = if next == '{' {
            let Some(relative_end) = chars[index + 2..]
                .iter()
                .position(|character| *character == '}')
            else {
                return Err(Error::Config(
                    "unterminated environment variable reference".into(),
                ));
            };
            let end = index + 2 + relative_end;
            (chars[index + 2..end].iter().collect::<String>(), end + 1)
        } else {
            let end = chars[index + 1..]
                .iter()
                .position(|character| !(character.is_ascii_alphanumeric() || *character == '_'))
                .map_or(chars.len(), |relative| index + 1 + relative);
            if end == index + 1 {
                output.push('$');
                index += 1;
                continue;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        };
        let resolved = std::env::var(&name)
            .map_err(|_| Error::Config(format!("environment variable {name} is not set")))?;
        output.push_str(&resolved);
        index = next_index;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            model: None,
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: 8192,
            reasoning_effort: None,
            hide_reasoning: false,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: 0.85,
            skill_dirs: Vec::new(),
            providers: default_local_providers(),
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
        }
    }

    #[test]
    fn context_window_prefers_config_override_and_provider_metadata() {
        let mut cfg = test_config();
        cfg.context_windows.insert("tiny".into(), 4096);
        let file: ConfigFile = serde_json::from_value(json!({
            "providers": {
                "omlx": {
                    "baseUrl": "http://127.0.0.1:8000/v1",
                    "api": "openai-completions",
                    "models": [{
                        "id": "local-model",
                        "contextWindow": 65536,
                        "maxTokens": 4096
                    }]
                }
            }
        }))
        .unwrap();
        cfg.apply(file);

        assert_eq!(cfg.context_window_for("tiny"), 4096);
        assert_eq!(cfg.context_window_for("openai:tiny"), 4096);
        assert_eq!(cfg.context_window_for("omlx:local-model"), 65_536);
        assert_eq!(cfg.max_tokens_for("omlx:local-model"), 4096);
        assert_eq!(cfg.context_window_for("claude-sonnet-4-5"), 200_000);
        assert_eq!(cfg.context_window_for("gpt-4o"), 128_000);
    }

    #[test]
    fn pi_style_provider_config_is_accepted() {
        let mut cfg = test_config();
        let file: ConfigFile = serde_json::from_value(json!({
            "providers": {
                "omlx": {
                    "baseUrl": "http://localhost:9000/v1",
                    "apiKey": "local-key",
                    "authHeader": true,
                    "compat": {
                        "supportsUsageInStreaming": false,
                        "maxTokensField": "max_tokens"
                    },
                    "models": [{"id": "qwen", "name": "Qwen local"}]
                }
            }
        }))
        .unwrap();
        cfg.apply(file);

        let provider = &cfg.providers["omlx"];
        assert_eq!(provider.base_url, "http://localhost:9000/v1");
        assert_eq!(provider.api_key.as_deref(), Some("local-key"));
        assert_eq!(provider.models[0].id, "qwen");
        assert!(!provider.compat.usage_in_stream());
        assert!(provider.compat.reasoning_content_on_assistant_messages());
    }

    #[test]
    fn reasoning_effort_accepts_levels_and_default() {
        let mut cfg = test_config();
        cfg.apply(serde_json::from_value(json!({"reasoning_effort": "high"})).unwrap());
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));

        cfg.apply(serde_json::from_value(json!({"reasoning_effort": "default"})).unwrap());
        assert_eq!(cfg.reasoning_effort, None);
    }

    #[test]
    fn reasoning_is_visible_unless_hidden() {
        let mut cfg = test_config();
        assert!(!cfg.hide_reasoning);

        cfg.apply(serde_json::from_value(json!({"hide_reasoning": true})).unwrap());
        assert!(cfg.hide_reasoning);
    }

    #[test]
    fn custom_prefix_leaves_colons_inside_model_id() {
        let cfg = test_config();
        let (name, _, model) = cfg
            .custom_provider_for("ollama:llama3.1:8b")
            .expect("preset should exist");
        assert_eq!(name, "ollama");
        assert_eq!(model, "llama3.1:8b");
    }

    #[test]
    fn config_value_escapes_literal_markers() {
        assert_eq!(
            resolve_config_value("$$money-$!bang").unwrap(),
            "$money-!bang"
        );
    }
}
