use std::collections::HashMap;
use std::path::PathBuf;

mod change;
mod loading;
mod schema;
mod storage;
mod types;

pub(crate) use change::{ConfigChange, ConfigChangeEffect, SkillDirectoryAction};
pub use loading::normalize_reasoning_effort;
pub(crate) use storage::resolve_config_value;
pub(crate) use types::UiColor;
pub use types::{ModelConfig, OpenAiCompatibility, ProviderConfig};

use loading::expand_home_path;
use storage::{object_field, validate_provider_name};

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.85;
const OPENAI_COMPLETIONS_API: &str = "openai-completions";

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
    pub(crate) accent_color: UiColor,
    /// Whether the TUI draws a transcript scroll bar.
    pub scroll_bar: bool,
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
    pub fn global_config_path(&self) -> PathBuf {
        self.home_dir.join("config.json")
    }

    pub fn project_config_path(&self) -> PathBuf {
        self.project_dir.join("config.json")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home_dir.join("sessions")
    }

    /// Tool scan order. Later directories override earlier ones on name
    /// collisions, so project tools win over global tools.
    pub fn tool_dirs(&self) -> [PathBuf; 2] {
        [self.home_dir.join("tools"), self.project_dir.join("tools")]
    }
}
