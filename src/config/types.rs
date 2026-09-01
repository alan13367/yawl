use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, de};
use unicode_width::UnicodeWidthStr;

use super::OPENAI_COMPLETIONS_API;
use super::schema::ProviderFile;

/// Ordered status-bar layout persisted in `config.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct StatusBarConfig {
    pub(crate) style: StatusBarStyle,
    pub(crate) separator: String,
    pub(crate) items: Vec<StatusBarItemConfig>,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            style: StatusBarStyle::Mixed,
            separator: "  ·  ".into(),
            items: StatusBarKind::ALL
                .into_iter()
                .map(StatusBarItemConfig::new)
                .collect(),
        }
    }
}

impl StatusBarConfig {
    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_status_text(&self.separator, 8, "status_bar.separator")?;
        let mut seen = std::collections::HashSet::new();
        for item in &self.items {
            if !seen.insert(item.kind) {
                return Err(format!(
                    "status_bar.items contains duplicate '{}' entries",
                    item.kind.as_str()
                ));
            }
            if let Some(label) = &item.label {
                validate_status_text(label, 32, "status_bar item label")?;
            }
        }
        Ok(())
    }
}

fn validate_status_text(value: &str, max_width: usize, field: &str) -> Result<(), String> {
    if value.chars().any(char::is_control) {
        return Err(format!("{field} must not contain control characters"));
    }
    if UnicodeWidthStr::width(value) > max_width {
        return Err(format!("{field} must be at most {max_width} columns wide"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusBarStyle {
    #[default]
    Mixed,
    Accent,
    Muted,
    Plain,
}

impl StatusBarStyle {
    pub(crate) const ALL: [Self; 4] = [Self::Mixed, Self::Accent, Self::Muted, Self::Plain];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Mixed => "mixed",
            Self::Accent => "accent",
            Self::Muted => "muted",
            Self::Plain => "plain",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusBarItemConfig {
    pub(crate) kind: StatusBarKind,
    #[serde(default)]
    pub(crate) format: StatusBarFormat,
    /// `None` uses the built-in label, `Some("")` removes it.
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) visibility: StatusBarVisibility,
}

impl StatusBarItemConfig {
    pub(crate) const fn new(kind: StatusBarKind) -> Self {
        Self {
            kind,
            format: StatusBarFormat::Current,
            label: None,
            visibility: StatusBarVisibility::Auto,
        }
    }
}

impl Default for StatusBarItemConfig {
    fn default() -> Self {
        Self::new(StatusBarKind::Model)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusBarKind {
    #[default]
    Model,
    Reasoning,
    Context,
    Cache,
    Elapsed,
    Queued,
    Steering,
    Goal,
    Pending,
    ActiveSubagents,
    FailedSubagents,
    ChildTokens,
}

impl StatusBarKind {
    pub(crate) const ALL: [Self; 12] = [
        Self::Model,
        Self::Reasoning,
        Self::Context,
        Self::Cache,
        Self::Elapsed,
        Self::Queued,
        Self::Steering,
        Self::Goal,
        Self::Pending,
        Self::ActiveSubagents,
        Self::FailedSubagents,
        Self::ChildTokens,
    ];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Reasoning => "reasoning",
            Self::Context => "context",
            Self::Cache => "cache",
            Self::Elapsed => "elapsed",
            Self::Queued => "queued",
            Self::Steering => "steering",
            Self::Goal => "goal",
            Self::Pending => "pending",
            Self::ActiveSubagents => "active_subagents",
            Self::FailedSubagents => "failed_subagents",
            Self::ChildTokens => "child_tokens",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Model => "Model",
            Self::Reasoning => "Reasoning effort",
            Self::Context => "Context usage",
            Self::Cache => "Prompt cache",
            Self::Elapsed => "Turn elapsed",
            Self::Queued => "Queued messages",
            Self::Steering => "Pending steering",
            Self::Goal => "Goal state",
            Self::Pending => "Pending settings",
            Self::ActiveSubagents => "Active subagents",
            Self::FailedSubagents => "Failed subagents",
            Self::ChildTokens => "Child token usage",
        }
    }

    pub(crate) const fn is_dynamic(self) -> bool {
        !matches!(self, Self::Model | Self::Context)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusBarFormat {
    #[default]
    Current,
    Compact,
    Detailed,
}

impl StatusBarFormat {
    pub(crate) const ALL: [Self; 3] = [Self::Current, Self::Compact, Self::Detailed];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Compact => "compact",
            Self::Detailed => "detailed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatusBarVisibility {
    #[default]
    Auto,
    Always,
}

impl StatusBarVisibility {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
        }
    }
}

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
    /// Whether this endpoint accepts OpenAI's `prompt_cache_key` request
    /// field. Disabled by default so local and generic compatible servers see
    /// the same requests as before.
    #[serde(alias = "supportsPromptCacheKey")]
    pub supports_prompt_cache_key: Option<bool>,
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
        if other.supports_prompt_cache_key.is_some() {
            self.supports_prompt_cache_key = other.supports_prompt_cache_key;
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

    pub fn prompt_cache_key_supported(&self) -> bool {
        self.supports_prompt_cache_key.unwrap_or(false)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn status_bar_defaults_match_the_original_item_order() {
        let layout = StatusBarConfig::default();

        assert_eq!(layout.style, StatusBarStyle::Mixed);
        assert_eq!(layout.separator, "  ·  ");
        assert_eq!(
            layout
                .items
                .iter()
                .map(|item| item.kind)
                .collect::<Vec<_>>(),
            StatusBarKind::ALL
        );
        assert!(layout.validate().is_ok());
    }

    #[test]
    fn status_bar_round_trips_label_states_and_empty_layouts() {
        let layout: StatusBarConfig = serde_json::from_value(json!({
            "style": "plain",
            "separator": "",
            "items": [
                {"kind": "model", "format": "compact", "label": null},
                {"kind": "cache", "format": "detailed", "label": "", "visibility": "always"}
            ]
        }))
        .expect("valid layout should deserialize");

        assert_eq!(layout.items[0].label, None);
        assert_eq!(layout.items[1].label.as_deref(), Some(""));
        assert_eq!(layout.items[1].visibility, StatusBarVisibility::Always);
        assert_eq!(
            serde_json::to_value(&layout).expect("layout should serialize")["items"][1]["label"],
            ""
        );
        assert!(serde_json::from_value::<StatusBarConfig>(json!({"items": []})).is_ok());
    }

    #[test]
    fn status_bar_rejects_duplicates_controls_and_overwide_text() {
        let mut duplicate = StatusBarConfig {
            items: vec![
                StatusBarItemConfig::new(StatusBarKind::Model),
                StatusBarItemConfig::new(StatusBarKind::Model),
            ],
            ..StatusBarConfig::default()
        };
        assert!(duplicate.validate().unwrap_err().contains("duplicate"));

        duplicate.items.truncate(1);
        duplicate.separator = "\x1b[31m".into();
        assert!(duplicate.validate().unwrap_err().contains("control"));

        duplicate.separator = " ".into();
        duplicate.items[0].label = Some("界".repeat(17));
        assert!(duplicate.validate().unwrap_err().contains("32 columns"));

        assert!(
            serde_json::from_value::<StatusBarConfig>(json!({
                "style": "neon",
                "items": []
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<StatusBarConfig>(json!({
                "items": [{"format": "compact"}]
            }))
            .is_err()
        );
    }
}
