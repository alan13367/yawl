//! Invocation-level project skill trust resolution. The question itself is
//! a setup screen drawn by the TUI.

use crate::config::Config;
use crate::tui::TrustAnswer;

pub(super) fn resolve(
    config: &mut Config,
    trust_override: bool,
    stdin_is_terminal: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if trust_override {
        config.set_project_skills_trusted(true);
        return apply_trusted_project_config(config);
    }
    let root = crate::trust::project_root(&crate::config::working_dir());
    match crate::trust::load(&config.home_dir, &root)? {
        crate::trust::Decision::Trusted => {
            config.set_project_skills_trusted(true);
            apply_trusted_project_config(config)
        }
        crate::trust::Decision::Denied => {
            config.set_project_skills_trusted(false);
            Ok(())
        }
        crate::trust::Decision::Unknown if project_settings(config).is_empty() => {
            config.set_project_skills_trusted(false);
            Ok(())
        }
        crate::trust::Decision::Unknown if stdin_is_terminal => {
            match crate::tui::ask_project_trust(config, &root, &project_settings(config))? {
                TrustAnswer::Permanent => {
                    crate::trust::store(&config.home_dir, &root, true)?;
                    config.set_project_skills_trusted(true);
                    apply_trusted_project_config(config)
                }
                TrustAnswer::Session => {
                    config.set_project_skills_trusted(true);
                    apply_trusted_project_config(config)
                }
                TrustAnswer::Deny => {
                    crate::trust::store(&config.home_dir, &root, false)?;
                    config.set_project_skills_trusted(false);
                    Ok(())
                }
            }
        }
        crate::trust::Decision::Unknown => {
            config.set_project_skills_trusted(false);
            eprintln!(
                "warning: project-controlled settings under {} are not trusted; project skills, \
                 tools, provider endpoints, and credentials from this repository are ignored. \
                 Pass --trust-project to enable them for this invocation",
                root.display()
            );
            Ok(())
        }
    }
}

/// Settles trust without asking or printing anything, for inspection
/// commands. Returns what trusting the project would enable when nobody has
/// decided yet; those settings stay ignored.
pub(super) fn resolve_quietly(
    config: &mut Config,
    trust_override: bool,
) -> Result<Vec<&'static str>, Box<dyn std::error::Error>> {
    let root = crate::trust::project_root(&crate::config::working_dir());
    let undecided = !trust_override
        && crate::trust::load(&config.home_dir, &root)? == crate::trust::Decision::Unknown;
    let pending = if undecided {
        project_settings(config)
    } else {
        Vec::new()
    };
    if pending.is_empty() {
        resolve(config, trust_override, false)?;
    } else {
        config.set_project_skills_trusted(false);
    }
    Ok(pending)
}

/// Applies the project config's provider endpoints and credentials once the
/// project itself is trusted.
fn apply_trusted_project_config(config: &mut Config) -> Result<(), Box<dyn std::error::Error>> {
    if config.project_config_restricted() {
        config.apply_project_trusted_config()?;
    }
    Ok(())
}

/// What trusting the project would enable, for the trust question.
fn project_settings(config: &Config) -> Vec<&'static str> {
    let mut found = Vec::new();
    if crate::skills::has_project_sources(config) {
        found.push("skills");
    }
    if crate::tools::has_project_tools(config) {
        found.push("tools");
    }
    if config.project_config_restricted() {
        found.push("provider settings");
    }
    found
}
