use std::collections::HashMap;

use serde::Deserialize;

use super::{ModelConfig, OpenAiCompatibility, UiColor};

/// On-disk shape of `config.json`. All fields are optional so the project
/// file can override only the keys it cares about.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ConfigFile {
    pub(super) model: Option<String>,
    pub(super) anthropic_base_url: Option<String>,
    pub(super) openai_base_url: Option<String>,
    pub(super) max_tokens: Option<u32>,
    pub(super) reasoning_effort: Option<String>,
    /// Whether reasoning content is omitted from terminal output.
    pub(super) hide_reasoning: Option<bool>,
    pub(super) accent_color: Option<UiColor>,
    /// Whether the transcript scroll bar is drawn in the TUI.
    pub(super) scroll_bar: Option<bool>,
    /// Compatibility with the brief two-color settings format.
    pub(super) status_bar_color: Option<UiColor>,
    /// Compatibility with the brief two-color settings format.
    pub(super) text_box_color: Option<UiColor>,
    /// Per-model context window overrides, e.g. {"my-local-model": 32768}.
    pub(super) context_windows: Option<HashMap<String, u64>>,
    /// Whether automatic context compaction is enabled.
    pub(super) auto_compact: Option<bool>,
    /// Fraction of the context window at which auto-compaction triggers.
    pub(super) compact_threshold: Option<f64>,
    pub(super) skill_dirs: Option<Vec<String>>,
    pub(super) providers: Option<HashMap<String, ProviderFile>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ProviderFile {
    #[serde(alias = "baseUrl")]
    pub(super) base_url: Option<String>,
    pub(super) api: Option<String>,
    #[serde(alias = "apiKey")]
    pub(super) api_key: Option<String>,
    #[serde(alias = "authHeader")]
    pub(super) auth_header: Option<bool>,
    pub(super) headers: Option<HashMap<String, String>>,
    pub(super) models: Option<Vec<ModelConfig>>,
    pub(super) compat: Option<OpenAiCompatibility>,
}
