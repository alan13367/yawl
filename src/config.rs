use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;

pub const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
pub const DEFAULT_COMPACT_THRESHOLD: f64 = 0.85;

/// On-disk shape of `config.json`. All fields optional so the project file
/// can override just the keys it cares about.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ConfigFile {
    model: Option<String>,
    anthropic_base_url: Option<String>,
    openai_base_url: Option<String>,
    max_tokens: Option<u32>,
    /// Per-model context window overrides, e.g. {"my-local-model": 32768}.
    context_windows: Option<HashMap<String, u64>>,
    /// Fraction of the context window at which auto-compaction triggers.
    compact_threshold: Option<f64>,
}

/// Effective configuration: defaults <- ~/.yawl/config.json <- ./.yawl/config.json.
#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    pub anthropic_base_url: String,
    pub openai_base_url: String,
    pub max_tokens: u32,
    pub context_windows: HashMap<String, u64>,
    pub compact_threshold: f64,
    /// `~/.yawl`
    pub home_dir: PathBuf,
    /// `./.yawl`
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
            model: DEFAULT_MODEL.to_string(),
            anthropic_base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            openai_base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            context_windows: HashMap::new(),
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
            home_dir,
            project_dir,
        };
        let global = cfg.home_dir.join("config.json");
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
        if let Some(v) = file.model {
            self.model = v;
        }
        if let Some(v) = file.anthropic_base_url {
            self.anthropic_base_url = v;
        }
        if let Some(v) = file.openai_base_url {
            self.openai_base_url = v;
        }
        if let Some(v) = file.max_tokens {
            self.max_tokens = v;
        }
        if let Some(map) = file.context_windows {
            self.context_windows.extend(map);
        }
        if let Some(v) = file.compact_threshold {
            self.compact_threshold = v.clamp(0.1, 0.99);
        }
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.home_dir.join("sessions")
    }

    /// Tool scan order; later directories override earlier ones on name
    /// collisions, so project tools win over global ones.
    pub fn tool_dirs(&self) -> [PathBuf; 2] {
        [self.home_dir.join("tools"), self.project_dir.join("tools")]
    }

    /// Context window for a model: explicit config override, else a
    /// name-based heuristic.
    pub fn context_window_for(&self, model: &str) -> u64 {
        let bare = model
            .strip_prefix("anthropic:")
            .or_else(|| model.strip_prefix("openai:"))
            .unwrap_or(model);
        if let Some(&w) = self
            .context_windows
            .get(model)
            .or_else(|| self.context_windows.get(bare))
        {
            return w;
        }
        if bare.starts_with("claude") {
            200_000
        } else {
            128_000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_window_prefers_config_override() {
        let mut cfg = Config {
            model: DEFAULT_MODEL.to_string(),
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: 1,
            context_windows: HashMap::new(),
            compact_threshold: 0.85,
            home_dir: PathBuf::new(),
            project_dir: PathBuf::new(),
        };
        cfg.context_windows.insert("tiny".into(), 4096);
        assert_eq!(cfg.context_window_for("tiny"), 4096);
        assert_eq!(cfg.context_window_for("openai:tiny"), 4096);
        assert_eq!(cfg.context_window_for("claude-sonnet-4-5"), 200_000);
        assert_eq!(cfg.context_window_for("gpt-4o"), 128_000);
    }
}
