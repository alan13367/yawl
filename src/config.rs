use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

mod change;
mod loading;
mod schema;
mod storage;
mod types;

pub(crate) use change::{ConfigChange, ConfigChangeEffect, SkillDirectoryAction};
pub use loading::normalize_reasoning_effort;
pub(crate) use loading::{
    bounded_message, expand_home_path, parse_bounded, validate_bounded, validate_file_value,
};
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
pub const DEFAULT_SUBAGENT_REQUEST_BUDGET: usize = 200;
pub const DEFAULT_SUBAGENT_TIMEOUT_SECS: u64 = 0;
pub const MAX_SUBAGENT_REQUEST_BUDGET: usize = 1000;
pub const MAX_SUBAGENT_TIMEOUT_SECS: u64 = 86_400;
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
    /// Highlight color for the selected row in menus and pickers. `None`
    /// follows the accent color.
    pub(crate) selection_color: Option<UiColor>,
    /// Whether the TUI draws a transcript scroll bar.
    pub scroll_bar: bool,
    /// Whether an idle transcript scroll bar hides itself after a pause.
    pub scroll_bar_auto_hide: bool,
    pub context_windows: HashMap<String, u64>,
    pub auto_compact: bool,
    pub compact_threshold: f64,
    /// Whether model-facing subagent orchestration tools are enabled.
    pub subagents: bool,
    /// Maximum number of subagents that may own active worker slots.
    pub max_subagents: usize,
    /// Default model for new subagents. `inherit` snapshots the parent model.
    pub subagent_model: String,
    /// Maximum model requests per subagent run before wrap-up steering,
    /// followed by a hard stop. `0` disables the budget.
    pub subagent_request_budget: usize,
    /// Wall-clock limit per subagent run in seconds. `0` disables it.
    pub subagent_timeout_secs: u64,
    /// Directories containing `NAME/SKILL.md` or `NAME.md` skills. Replacing
    /// this list programmatically replaces the configured skill roots.
    pub skill_dirs: Vec<PathBuf>,
    /// Skill directories from defaults plus the global config, before any
    /// project override. Project-controlled roots stay separate so trust can
    /// gate them without suppressing global skills.
    pub(crate) global_skill_dirs: Vec<PathBuf>,
    /// A `skill_dirs` override supplied by `./.yawl/config.json`.
    pub(crate) project_skill_dirs: Option<Vec<PathBuf>>,
    /// Whether project-controlled skill sources may be read this invocation.
    pub(crate) project_skills_trusted: bool,
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
    pub fn set_project_skills_trusted(&mut self, trusted: bool) {
        self.project_skills_trusted = trusted;
    }

    pub fn project_skills_trusted(&self) -> bool {
        self.project_skills_trusted
    }

    pub(crate) fn has_project_skill_override(&self) -> bool {
        self.skill_dir_sources().1.is_some()
    }

    pub(crate) fn skill_dir_sources(&self) -> (&[PathBuf], Option<&[PathBuf]>) {
        let loaded_effective = self
            .project_skill_dirs
            .as_deref()
            .unwrap_or(&self.global_skill_dirs);
        if self.skill_dirs.as_slice() != loaded_effective {
            (&self.skill_dirs, None)
        } else {
            (&self.global_skill_dirs, self.project_skill_dirs.as_deref())
        }
    }

    /// The menu selection highlight color: the explicit `selection_color`
    /// when set, otherwise the accent color.
    pub(crate) fn effective_selection_color(&self) -> UiColor {
        self.selection_color.unwrap_or(self.accent_color)
    }

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

    /// Subagent preset scan order. Later directories override earlier ones
    /// on name collisions, so project presets win over global presets.
    pub fn agent_dirs(&self) -> [PathBuf; 2] {
        [
            self.home_dir.join("agents"),
            self.project_dir.join("agents"),
        ]
    }

    /// Directory containing sessions scoped to the project that owns `cwd`.
    pub fn project_sessions_dir(&self, cwd: &Path) -> PathBuf {
        self.sessions_dir().join("projects").join(project_key(cwd))
    }

    /// Session storage layout for the project that owns `cwd`.
    pub fn session_dirs(&self, cwd: &Path) -> SessionDirs {
        let projects = self.sessions_dir().join("projects");
        let project = self.project_sessions_dir(cwd);
        let mut search = vec![project.clone()];
        if let Ok(entries) = fs::read_dir(&projects) {
            let mut rest: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir() && path != &project)
                .collect();
            rest.sort();
            search.extend(rest);
        }
        SessionDirs { project, search }
    }
}

/// Session storage directories for one invocation.
#[derive(Debug, Clone)]
pub struct SessionDirs {
    /// Sessions scoped to the current project.
    pub project: PathBuf,
    /// Every project directory to search for an explicit session id. Session
    /// ids must be unique across these directories; search order is not a
    /// conflict-resolution rule.
    pub search: Vec<PathBuf>,
}

/// Returns the canonical working directory that scopes new sessions,
/// defaulting to `.` when the process environment cannot be queried.
pub fn working_dir() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    fs::canonicalize(&cwd).unwrap_or(cwd)
}

/// Stable directory key for a project: sanitized canonical basename plus a
/// short FNV-1a digest of the full canonical path, so same-named directories
/// stay distinct.
fn project_key(cwd: &Path) -> String {
    let canonical = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let digest = fnv1a(canonical.as_os_str().as_encoded_bytes());
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_project_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string());
    format!("{name}-{:08x}", digest as u32)
}

/// Lowercases a directory name and collapses it to `[a-z0-9-]`, trimming `-`
/// and capping the length so keys stay readable and filesystem-safe.
fn sanitize_project_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars().flat_map(char::to_lowercase) {
        let mapped = if ch.is_ascii_alphanumeric() { ch } else { '-' };
        if mapped != '-' || !sanitized.ends_with('-') {
            sanitized.push(mapped);
        }
    }
    let mut key = sanitized.trim_matches('-').to_string();
    if key.len() > 24 {
        key.truncate(24);
        key = key.trim_end_matches('-').to_string();
    }
    key
}

/// FNV-1a digest over the path bytes, hand-rolled to keep the dependency
/// count low. Only the low 32 bits are used for directory keys.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
impl Config {
    pub(crate) fn test_default() -> Self {
        Self {
            model: None,
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            selection_color: None,
            scroll_bar: true,
            scroll_bar_auto_hide: true,
            context_windows: HashMap::new(),
            auto_compact: true,
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            subagents: false,
            max_subagents: DEFAULT_MAX_SUBAGENTS,
            subagent_model: DEFAULT_SUBAGENT_MODEL.to_string(),
            subagent_request_budget: DEFAULT_SUBAGENT_REQUEST_BUDGET,
            subagent_timeout_secs: DEFAULT_SUBAGENT_TIMEOUT_SECS,
            skill_dirs: Vec::new(),
            global_skill_dirs: Vec::new(),
            project_skill_dirs: None,
            project_skills_trusted: false,
            providers: HashMap::new(),
            setup_skipped: false,
            anthropic_api_key: None,
            openai_api_key: None,
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_keys_sanitize_names_and_split_same_named_directories() {
        let first = project_key(Path::new("/srv/alpha/My Project"));
        let second = project_key(Path::new("/srv/beta/My Project"));
        assert!(first.starts_with("my-project-"));
        assert_ne!(first, second);

        let messy = project_key(Path::new("/srv/---Messy   Project---Name###"));
        assert!(messy.starts_with("messy-project-name-"));

        let empty = project_key(Path::new("/srv/---###"));
        assert!(empty.starts_with("project-"));
    }

    #[test]
    fn project_keys_match_through_symlinks() {
        let root = std::env::temp_dir().join(format!("yawl-config-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("real")).expect("test directory");
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).expect("symlink");

        assert_eq!(
            project_key(&root.join("real")),
            project_key(&root.join("link"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn session_dirs_search_current_and_other_projects() {
        let root = std::env::temp_dir().join(format!("yawl-config-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let config = Config {
            home_dir: root.clone(),
            ..Config::test_default()
        };
        let projects = config.sessions_dir().join("projects");
        std::fs::create_dir_all(projects.join("bbbb")).expect("other project");
        std::fs::create_dir_all(projects.join("aaaa")).expect("other project");

        let cwd = Path::new("/srv/yawl");
        let dirs = config.session_dirs(cwd);

        assert_eq!(dirs.project, projects.join(project_key(cwd)));
        assert_eq!(dirs.search[0], dirs.project);
        assert_eq!(dirs.search[1], projects.join("aaaa"));
        assert_eq!(dirs.search[2], projects.join("bbbb"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
