use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, de};

use super::OPENAI_COMPLETIONS_API;
use super::schema::ProviderFile;

/// Search service used by the built-in `web_search` tool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WebSearchProvider {
    /// Keyless search through DuckDuckGo's HTML results page.
    #[default]
    DuckDuckGo,
    /// Brave Web Search API.
    Brave,
    /// Firecrawl Search API.
    Firecrawl,
}

impl WebSearchProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Brave => "brave",
            Self::Firecrawl => "firecrawl",
        }
    }
}

impl fmt::Display for WebSearchProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WebSearchProvider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "duckduckgo" => Ok(Self::DuckDuckGo),
            "brave" => Ok(Self::Brave),
            "firecrawl" => Ok(Self::Firecrawl),
            _ => Err("web_search_provider must be duckduckgo, brave, or firecrawl".to_string()),
        }
    }
}

/// RGB color used by the terminal UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UiColor {
    pub(crate) red: u8,
    pub(crate) green: u8,
    pub(crate) blue: u8,
}

impl UiColor {
    pub(crate) const WHITE: Self = Self::new(238, 238, 238);

    pub(crate) const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        let normalized = value.trim().to_ascii_lowercase();
        let named = match normalized.as_str() {
            "white" => Some(Self::WHITE),
            "gray" | "grey" => Some(Self::new(148, 148, 158)),
            "red" => Some(Self::new(235, 111, 146)),
            "orange" => Some(Self::new(240, 160, 96)),
            "yellow" => Some(Self::new(232, 202, 118)),
            "green" => Some(Self::new(139, 213, 162)),
            "cyan" => Some(Self::new(116, 199, 213)),
            "blue" => Some(Self::new(117, 169, 255)),
            "purple" => Some(Self::new(190, 149, 255)),
            "pink" => Some(Self::new(238, 148, 200)),
            _ => None,
        };
        if let Some(color) = named {
            return Ok(color);
        }
        let hex = normalized.strip_prefix('#').unwrap_or(&normalized);
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("color must be a palette name or #RRGGBB".into());
        }
        let component = |range| {
            u8::from_str_radix(&hex[range], 16)
                .map_err(|_| "color must be a palette name or #RRGGBB".to_string())
        };
        Ok(Self::new(
            component(0..2)?,
            component(2..4)?,
            component(4..6)?,
        ))
    }

    /// Parses a selection-color value: `accent` yields `None` (follow the
    /// accent color), anything else must be a palette name or `#RRGGBB`.
    pub(crate) fn parse_selection(value: &str) -> Result<Option<Self>, String> {
        if value.trim().eq_ignore_ascii_case("accent") {
            return Ok(None);
        }
        Self::parse(value).map(Some)
    }

    /// Stored form of a selection-color value, the inverse of
    /// [`UiColor::parse_selection`].
    pub(crate) fn selection_config_value(selection: Option<Self>) -> String {
        selection.map_or_else(|| "accent".into(), Self::config_value)
    }

    pub(crate) fn config_value(self) -> String {
        let named = [
            ("white", Self::WHITE),
            ("gray", Self::new(148, 148, 158)),
            ("red", Self::new(235, 111, 146)),
            ("orange", Self::new(240, 160, 96)),
            ("yellow", Self::new(232, 202, 118)),
            ("green", Self::new(139, 213, 162)),
            ("cyan", Self::new(116, 199, 213)),
            ("blue", Self::new(117, 169, 255)),
            ("purple", Self::new(190, 149, 255)),
            ("pink", Self::new(238, 148, 200)),
        ];
        named
            .into_iter()
            .find_map(|(name, color)| (color == self).then_some(name.to_string()))
            .unwrap_or_else(|| format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue))
    }
}

impl<'de> Deserialize<'de> for UiColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

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
    /// Input kinds accepted by this model, such as `text` and `image`.
    #[serde(default)]
    pub input: Vec<String>,
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
    pub(super) fn openai_compatible(base_url: &str) -> Self {
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

    pub(super) fn apply(&mut self, file: ProviderFile) {
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
