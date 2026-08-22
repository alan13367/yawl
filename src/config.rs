use std::collections::HashMap;
use std::path::PathBuf;

mod change;
mod loading;
mod schema;
mod storage;
mod types;

pub(crate) use change::{ConfigChange, ConfigChangeEffect, SkillDirectoryAction};
pub(crate) use loading::expand_home_path;
pub use loading::normalize_reasoning_effort;
pub(crate) use schema::validate_file_shape;
pub(crate) use storage::{read_json_object, resolve_config_value, write_json_object};
pub(crate) use types::UiColor;
pub use types::{ModelConfig, OpenAiCompatibility, ProviderConfig};

use storage::{object_field, validate_provider_name};

pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.85;
pub const DEFAULT_MAX_SUBAGENTS: usize = 3;
pub const DEFAULT_SUBAGENT_MODEL: &str = "inherit";
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
    /// Whether model-facing subagent orchestration tools are enabled.
    pub subagents: bool,
    /// Maximum number of subagents that may own active worker slots.
    pub max_subagents: usize,
    /// Default model for new subagents. `inherit` snapshots the parent model.
    pub subagent_model: String,
    /// Directories containing `NAME/SKILL.md` or `NAME.md` skills.
    pub skill_dirs: Vec<PathBuf>,
    pub providers: HashMap<String, ProviderConfig>,
    /// Whether the user explicitly skipped onboarding, suppressing the
    /// first-run setup prompt.
    pub setup_skipped: bool,
    /// Anthropic key stored in config, used when `ANTHROPIC_API_KEY` is unset.
    pub anthropic_api_key: Option<String>,
    /// OpenAI key stored in config, used when `OPENAI_API_KEY` is unset.
    pub openai_api_key: Option<String>,
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
