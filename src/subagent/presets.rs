//! Subagent presets: named spawn profiles discovered from
//! `~/.yawl/agents/` and `./.yawl/agents/` as JSON files, layered over a
//! small set of bundled defaults. A preset pins the child's model, tool
//! allowlist, and an extra role instruction so `subagent_spawn` can request
//! a specialist instead of a generic child.

use std::path::Path;

use serde::Deserialize;

use crate::config::Config;
use crate::tools::exec::valid_tool_name;

/// One spawn profile. `model` follows the spawn precedence chain when
/// absent; `tools` grants everything when absent; `prompt` is appended to
/// the subagent role block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentPreset {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) model: Option<String>,
    pub(crate) tools: Option<Vec<String>>,
    pub(crate) prompt: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PresetFile {
    description: Option<String>,
    model: Option<String>,
    tools: Option<Vec<String>>,
    prompt: Option<String>,
}

/// Presets that ship with Yawl. File presets with the same name replace
/// them, so users can override `scout` by dropping `~/.yawl/agents/scout.json`.
pub(crate) fn bundled() -> Vec<AgentPreset> {
    vec![AgentPreset {
        name: "scout".into(),
        description: "Read-only inspection of existing files; cannot create or modify files".into(),
        model: None,
        // `shell` is intentionally excluded. Even commands that look like
        // searches can contain redirection or invoke mutating subprocesses,
        // so a prompt instruction cannot make the shell read-only.
        tools: Some(vec!["read_file".into(), "read_skill".into()]),
        prompt: Some(
            "You are a read-only research specialist. Inspect the files named in the delegated \
             task and report findings with exact paths and line references. Never modify files \
             or run commands; keep the investigation short and return a concise report. If the \
             task does not provide enough file paths, report what additional scope is needed."
                .into(),
        ),
    }]
}

/// Loads bundled presets, then every `NAME.json` under the agent
/// directories in scan order (later directories override earlier ones on
/// name collisions, so project presets win). Malformed files produce
/// warnings instead of failing the spawn path.
pub(crate) fn discover(config: &Config) -> (Vec<AgentPreset>, Vec<String>) {
    let mut presets = bundled();
    let mut warnings = Vec::new();
    for dir in config.agent_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warnings.push(format!("{}: {error}", dir.display()));
                continue;
            }
        };
        let mut paths = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string();
            match load_preset(&path, &name) {
                Ok(Some(preset)) => {
                    if let Some(existing) = presets.iter().position(|p| p.name == preset.name) {
                        presets[existing] = preset;
                    } else {
                        presets.push(preset);
                    }
                }
                Ok(None) => warnings.push(format!(
                    "{}: preset name must be 1-64 ASCII letters, digits, '_' or '-'",
                    path.display()
                )),
                Err(reason) => warnings.push(format!("{}: {reason}", path.display())),
            }
        }
    }
    (presets, warnings)
}

fn load_preset(path: &Path, name: &str) -> Result<Option<AgentPreset>, String> {
    if !valid_tool_name(name) {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("unreadable preset: {error}"))?;
    let file: PresetFile =
        serde_json::from_str(&text).map_err(|error| format!("bad preset json: {error}"))?;
    if let Some(model) = file.model.as_deref()
        && model.trim().is_empty()
    {
        return Err("model must not be empty when set".into());
    }
    let tools = file.tools.map(|tools| {
        tools
            .into_iter()
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect::<Vec<_>>()
    });
    if tools.as_ref().is_some_and(|tools| tools.is_empty()) {
        return Err("tools must name at least one tool when set".into());
    }
    Ok(Some(AgentPreset {
        name: name.into(),
        description: file.description.unwrap_or_else(|| name.into()),
        model: file.model,
        tools,
        prompt: file.prompt.filter(|prompt| !prompt.trim().is_empty()),
    }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn test_config(home: &Path, project: &Path) -> Config {
        Config {
            subagents: true,
            home_dir: home.to_path_buf(),
            project_dir: project.to_path_buf(),
            ..Config::test_default()
        }
    }

    #[test]
    fn bundled_scout_is_available_without_any_files() {
        let root = std::env::temp_dir().join(format!("yawl-presets-{}", std::process::id()));
        let config = test_config(&root, &root);
        let (presets, warnings) = discover(&config);
        assert!(warnings.is_empty());
        assert_eq!(presets.len(), 1);
        assert_eq!(presets[0].name, "scout");
        assert_eq!(
            presets[0].tools.as_deref(),
            Some(["read_file".to_string(), "read_skill".to_string()].as_slice())
        );
    }

    #[test]
    fn file_presets_are_discovered_and_project_files_override() {
        let root = std::env::temp_dir().join(format!("yawl-presets-merge-{}", std::process::id()));
        let home = root.join("home/.yawl");
        let project = root.join("project/.yawl");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(home.join("agents")).expect("home agents dir");
        std::fs::create_dir_all(project.join("agents")).expect("project agents dir");
        std::fs::write(
            home.join("agents/reviewer.json"),
            r#"{"description":"home reviewer","model":"openai:gpt-4o-mini","prompt":"review code"}"#,
        )
        .expect("home preset");
        std::fs::write(
            home.join("agents/scout.json"),
            r#"{"description":"overridden at home","tools":["read_file"]}"#,
        )
        .expect("home scout override");
        std::fs::write(
            project.join("agents/scout.json"),
            r#"{"description":"project scout","tools":["read_file","shell"]}"#,
        )
        .expect("project scout override");
        std::fs::write(home.join("agents/broken.json"), "{ not json").expect("broken preset");

        let config = test_config(&root.join("home/.yawl"), &root.join("project/.yawl"));
        let (presets, warnings) = discover(&config);

        let scout = presets.iter().find(|p| p.name == "scout").expect("scout");
        assert_eq!(scout.description, "project scout");
        let reviewer = presets
            .iter()
            .find(|p| p.name == "reviewer")
            .expect("reviewer");
        assert_eq!(reviewer.description, "home reviewer");
        assert_eq!(reviewer.model.as_deref(), Some("openai:gpt-4o-mini"));
        assert_eq!(reviewer.tools, None);
        assert_eq!(reviewer.prompt.as_deref(), Some("review code"));
        assert_eq!(warnings.len(), 1, "malformed files warn, not fail");
        assert!(warnings[0].contains("broken.json"), "{warnings:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_preset_fields_are_rejected_with_reasons() {
        let root =
            std::env::temp_dir().join(format!("yawl-presets-invalid-{}", std::process::id()));
        let home = root.join(".yawl");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(home.join("agents")).expect("agents dir");
        std::fs::write(home.join("agents/bad name.json"), r#"{}"#).expect("bad name");
        std::fs::write(home.join("agents/empty-tools.json"), r#"{"tools":["  "]}"#)
            .expect("empty tools");
        std::fs::write(home.join("agents/blank-model.json"), r#"{"model":" "}"#)
            .expect("blank model");

        let config = test_config(&root.join(".yawl"), &root.join("nonexistent"));
        let (presets, warnings) = discover(&config);

        assert!(presets.iter().all(|preset| preset.name != "bad name"));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().any(|warning| warning.contains("bad name")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("empty-tools"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("blank-model"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
