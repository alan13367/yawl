//! The skill that teaches other agent harnesses (Claude Code, Codex,
//! OpenCode, and anything reading Agent Skills) to delegate to Yawl. It is
//! compiled in, so the instructions always match the installed binary.

use std::path::{Path, PathBuf};

use crate::error::Error;

const SKILL: &str = include_str!("skill_export/SKILL.md");
pub(crate) const SKILL_NAME: &str = "yawl";

/// Targets `--install-skill` accepts by name, each with the skills
/// directory it writes into.
const TARGETS: &str = "claude, codex, opencode, agents, all, or a skills directory path";

/// The skill text, stamped with this binary's version.
pub(crate) fn text() -> String {
    SKILL.replace("{{version}}", env!("CARGO_PKG_VERSION"))
}

/// Writes the skill into each skills directory `target` names and returns
/// the files written.
pub(crate) fn install(target: &str) -> Result<Vec<PathBuf>, Error> {
    let dirs = skill_dirs(target, &Environment::current())?;
    let text = text();
    dirs.iter()
        .map(|dir| write_skill(&dir.join(SKILL_NAME), &text))
        .collect()
}

/// Where each harness looks for user-level skills.
struct Environment {
    home: Option<PathBuf>,
    claude_config_dir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

impl Environment {
    fn current() -> Self {
        let non_empty = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: std::env::home_dir(),
            claude_config_dir: non_empty("CLAUDE_CONFIG_DIR"),
            xdg_config_home: non_empty("XDG_CONFIG_HOME"),
        }
    }

    fn home(&self) -> Result<&Path, Error> {
        self.home
            .as_deref()
            .ok_or_else(|| Error::Config("cannot find the home directory".into()))
    }

    fn claude(&self) -> Result<PathBuf, Error> {
        Ok(match &self.claude_config_dir {
            Some(dir) => dir.join("skills"),
            None => self.home()?.join(".claude/skills"),
        })
    }

    /// The cross-harness directory Codex reads, as do OpenCode and others.
    fn agents(&self) -> Result<PathBuf, Error> {
        Ok(self.home()?.join(".agents/skills"))
    }

    fn opencode(&self) -> Result<PathBuf, Error> {
        Ok(match &self.xdg_config_home {
            Some(dir) => dir.join("opencode/skills"),
            None => self.home()?.join(".config/opencode/skills"),
        })
    }
}

fn skill_dirs(target: &str, environment: &Environment) -> Result<Vec<PathBuf>, Error> {
    Ok(match target {
        "claude" | "claude-code" => vec![environment.claude()?],
        "codex" | "agents" => vec![environment.agents()?],
        "opencode" => vec![environment.opencode()?],
        // OpenCode also reads both of these, so `all` needs no third copy.
        "all" => vec![environment.claude()?, environment.agents()?],
        path if path.contains(['/', '\\']) || path.starts_with('.') || path.starts_with('~') => {
            vec![expand_home(path, environment)?]
        }
        _ => {
            return Err(Error::Invalid(format!(
                "unknown skill target '{target}'; expected {TARGETS}"
            )));
        }
    })
}

fn expand_home(path: &str, environment: &Environment) -> Result<PathBuf, Error> {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => Ok(environment.home()?.join(rest)),
        None if path == "~" => Ok(environment.home()?.to_path_buf()),
        None => Ok(PathBuf::from(path)),
    }
}

/// Writes `SKILL.md` into `dir` through a temporary file, so a harness
/// scanning skills never reads half a file.
fn write_skill(dir: &Path, text: &str) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("SKILL.md");
    let temporary = dir.join(format!(".SKILL.md.{}.tmp", std::process::id()));
    std::fs::write(&temporary, text)?;
    if let Err(error) = crate::paths::replace_file(&temporary, &path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(home: &str) -> Environment {
        Environment {
            home: Some(PathBuf::from(home)),
            claude_config_dir: None,
            xdg_config_home: None,
        }
    }

    #[test]
    fn named_targets_map_to_each_harness_directory() {
        let env = environment("/home/u");
        let dirs = |target| skill_dirs(target, &env).expect("known target");
        assert_eq!(dirs("claude"), [PathBuf::from("/home/u/.claude/skills")]);
        assert_eq!(dirs("codex"), [PathBuf::from("/home/u/.agents/skills")]);
        assert_eq!(
            dirs("opencode"),
            [PathBuf::from("/home/u/.config/opencode/skills")]
        );
        assert_eq!(dirs("all").len(), 2);
        assert_eq!(dirs("~/skills"), [PathBuf::from("/home/u").join("skills")]);
        assert_eq!(
            dirs("./.claude/skills"),
            [PathBuf::from("./.claude/skills")]
        );
        assert!(skill_dirs("vim", &env).is_err());
    }

    #[test]
    fn harness_config_overrides_are_respected() {
        let env = Environment {
            claude_config_dir: Some(PathBuf::from("/cfg/claude")),
            xdg_config_home: Some(PathBuf::from("/xdg")),
            ..environment("/home/u")
        };
        assert_eq!(
            skill_dirs("claude", &env).unwrap(),
            [PathBuf::from("/cfg/claude/skills")]
        );
        assert_eq!(
            skill_dirs("opencode", &env).unwrap(),
            [PathBuf::from("/xdg/opencode/skills")]
        );
    }

    #[test]
    fn skill_has_valid_frontmatter_and_the_current_version() {
        let text = text();
        let frontmatter = text
            .strip_prefix("---\n")
            .and_then(|rest| rest.split_once("\n---\n"))
            .map(|(frontmatter, _)| frontmatter)
            .expect("SKILL.md starts with frontmatter");
        assert!(frontmatter.lines().any(|line| line == "name: yawl"));
        let description = frontmatter
            .lines()
            .find_map(|line| line.strip_prefix("description: "))
            .expect("description");
        assert!(description.len() <= 1024, "Agent Skills cap descriptions");
        assert!(!text.contains("{{version}}"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn every_flag_the_skill_names_is_one_the_cli_accepts() {
        // Flags of other commands the examples use.
        const OTHER_COMMANDS: [&str; 1] = ["--staged"];
        let text = text();
        let mut flags = text
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .filter(|word| {
                word.strip_prefix("--")
                    .is_some_and(|name| name.starts_with(|first: char| first.is_ascii_alphabetic()))
            })
            .collect::<Vec<_>>();
        flags.sort_unstable();
        flags.dedup();
        flags.retain(|flag| !OTHER_COMMANDS.contains(flag));
        assert!(flags.contains(&"--no-stdin"), "the scan finds flags");
        for flag in flags {
            assert!(
                crate::cli::HELP.contains(flag),
                "SKILL.md mentions {flag}, which --help does not document"
            );
        }
    }

    #[test]
    fn install_writes_the_skill_and_replaces_an_old_copy() {
        let root = crate::test_support::unique_temp_dir("skill-install");
        let target = root.join("skills");
        let target = target.to_str().expect("utf-8 temp path");
        let first = install(target).expect("install");
        std::fs::write(&first[0], "old").unwrap();
        let second = install(target).expect("reinstall");
        assert_eq!(first, second);
        assert_eq!(std::fs::read_to_string(&second[0]).unwrap(), text());
        assert!(second[0].ends_with("yawl/SKILL.md"));
        let _ = std::fs::remove_dir_all(root);
    }
}
