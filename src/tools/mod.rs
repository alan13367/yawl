//! Tool registry: builtins plus exec tools discovered from disk.
//!
//! The registry is rescanned every agent-loop iteration, so a tool the model
//! just wrote is usable on its next turn. On name collisions the last scan
//! wins (builtins < `~/.yawl/tools` < `./.yawl/tools`).

pub mod exec;

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::provider::ToolSpec;
use crate::subagent::presets::discover as discover_presets;
use crate::subagent::{AgentPreset, RunOrigin, SubagentManager};

pub use exec::DescribeCache;

/// Cap on tool result size fed back to the model.
const MAX_RESULT_CHARS: usize = 60_000;
/// Cap on `read_file` input size. Anything larger truncates to
/// `MAX_RESULT_CHARS` anyway, so reading it in full only wastes memory.
const MAX_READ_FILE_BYTES: u64 = 1024 * 1024;
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 120;

enum ToolImpl {
    Shell,
    ReadFile,
    WriteFile,
    EditFile,
    Exec(exec::ExecTool),
    Subagent(SubagentTool),
}

#[derive(Clone, Copy)]
enum SubagentTool {
    Spawn,
    Send,
    Wait,
    Cancel,
    List,
}

struct SubagentContext {
    manager: SubagentManager,
    config: Config,
    parent_model: String,
    presets: Vec<AgentPreset>,
}

struct ToolEntry {
    spec: ToolSpec,
    imp: ToolImpl,
}

pub struct ToolOutcome {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutcome {
    fn error(msg: impl Into<String>) -> ToolOutcome {
        ToolOutcome {
            content: msg.into(),
            is_error: true,
        }
    }

    fn ok(content: String) -> ToolOutcome {
        ToolOutcome {
            content,
            is_error: false,
        }
    }
}

pub struct Registry {
    entries: Vec<ToolEntry>,
    pub warnings: Vec<String>,
    subagents: Option<SubagentContext>,
}

impl Registry {
    /// Scans builtins + exec tool directories. Called every loop iteration;
    /// `cache` avoids respawning `--describe` for unchanged tools.
    pub fn scan(config: &Config, cache: &mut DescribeCache) -> Registry {
        let mut registry = Registry {
            entries: builtins(),
            warnings: Vec::new(),
            subagents: None,
        };
        for dir in config.tool_dirs() {
            let (tools, warnings) = exec::scan_dir(&dir, cache);
            registry.warnings.extend(warnings);
            for tool in tools {
                if RESERVED_TOOL_NAMES.contains(&tool.spec.name.as_str()) {
                    registry.warnings.push(format!(
                        "{}: tool name '{}' is reserved by Yawl",
                        tool.path.display(),
                        tool.spec.name
                    ));
                    continue;
                }
                registry.insert(ToolEntry {
                    spec: tool.spec.clone(),
                    imp: ToolImpl::Exec(tool),
                });
            }
        }
        registry
    }

    pub(crate) fn scan_with_subagents(
        config: &Config,
        cache: &mut DescribeCache,
        manager: SubagentManager,
        parent_model: &str,
    ) -> Registry {
        let mut registry = Self::scan(config, cache);
        let (presets, warnings) = discover_presets(config);
        registry.warnings.extend(warnings);
        registry.entries.extend(subagent_tools(&presets));
        registry.subagents = Some(SubagentContext {
            manager,
            config: config.clone(),
            parent_model: parent_model.to_string(),
            presets,
        });
        registry
    }

    /// Drops every entry whose name is not listed. Used for preset
    /// subagents, whose tool allowlist is enforced at scan time.
    pub(crate) fn retain_names(&mut self, names: &[String]) {
        self.entries
            .retain(|entry| names.contains(&entry.spec.name));
    }

    fn insert(&mut self, entry: ToolEntry) {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.spec.name == entry.spec.name)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|e| e.spec.clone()).collect()
    }

    /// Name, description, and origin for `/tools` and `--list-tools`.
    pub fn describe_all(&self) -> Vec<(String, String, String)> {
        self.entries
            .iter()
            .map(|e| {
                let origin = match &e.imp {
                    ToolImpl::Exec(t) => t.path.display().to_string(),
                    ToolImpl::Subagent(_) => "orchestration".to_string(),
                    _ => "builtin".to_string(),
                };
                (e.spec.name.clone(), e.spec.description.clone(), origin)
            })
            .collect()
    }

    pub fn execute(&self, name: &str, args_json: &str, session_id: &str) -> ToolOutcome {
        let Some(entry) = self.entries.iter().find(|e| e.spec.name == name) else {
            return ToolOutcome::error(format!(
                "unknown tool '{name}'; available: {}",
                self.entries
                    .iter()
                    .map(|e| e.spec.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        };
        let args: Value = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return ToolOutcome::error(format!("invalid tool arguments json: {e}")),
        };
        let mut outcome = match &entry.imp {
            ToolImpl::Shell => shell(&args),
            ToolImpl::ReadFile => read_file(&args),
            ToolImpl::WriteFile => write_file(&args),
            ToolImpl::EditFile => edit_file(&args),
            ToolImpl::Exec(tool) => {
                let (content, is_error) = exec::invoke(tool, args_json, session_id);
                ToolOutcome { content, is_error }
            }
            ToolImpl::Subagent(tool) => self.execute_subagent(*tool, &args),
        };
        truncate_result(&mut outcome.content);
        if outcome.content.is_empty() {
            outcome.content = if outcome.is_error {
                "(no output)".to_string()
            } else {
                "(no output; command succeeded)".to_string()
            };
        }
        outcome
    }

    fn execute_subagent(&self, tool: SubagentTool, args: &Value) -> ToolOutcome {
        let Some(context) = &self.subagents else {
            return ToolOutcome::error("subagent orchestration is not available");
        };
        let result = match tool {
            SubagentTool::Spawn => {
                if args.get("model").is_some() {
                    return ToolOutcome::error(
                        "'model' is not accepted; subagents use the configured model or inherit the active parent model",
                    );
                }
                let prompt = str_arg(args, "prompt").map_err(|error| error.content);
                let required_tools = string_array(args, "required_tools");
                let name = match args.get("name") {
                    Some(value) => value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "'name' must be a string when provided".to_string()),
                    None => Ok(String::new()),
                };
                prompt.and_then(|prompt| {
                    required_tools.and_then(|required_tools| {
                        name.and_then(|name| {
                            let preset: Option<&AgentPreset> = match args.get("agent") {
                                Some(value) => {
                                    let agent = value.as_str().ok_or_else(|| {
                                        "'agent' must be a string when provided".to_string()
                                    })?;
                                    Some(
                                        context
                                            .presets
                                            .iter()
                                            .find(|preset| preset.name == agent)
                                            .ok_or_else(|| {
                                                format!(
                                                    "unknown agent '{agent}'; available: {}",
                                                    context
                                                        .presets
                                                        .iter()
                                                        .map(|preset| preset.name.as_str())
                                                        .collect::<Vec<_>>()
                                                        .join(", ")
                                                )
                                            })?,
                                    )
                                }
                                None => None,
                            };
                            if let Some(preset) = preset {
                                validate_preset_capabilities(preset, &required_tools)?;
                            }
                            let supplied_name = (!name.trim().is_empty()).then_some(name.as_str());
                            context
                                .manager
                                .spawn(
                                    context.config.clone(),
                                    &context.parent_model,
                                    supplied_name,
                                    prompt,
                                    preset,
                                )
                                .map(|id| format!("started {id}"))
                        })
                    })
                })
            }
            SubagentTool::Send => {
                let id = str_arg(args, "id").map_err(|error| error.content);
                let message = str_arg(args, "message").map_err(|error| error.content);
                id.and_then(|id| {
                    message.and_then(|message| context.manager.send(id, message, RunOrigin::Model))
                })
            }
            SubagentTool::Wait => string_array(args, "ids").and_then(|ids| {
                let timeout = match args.get("timeout_secs") {
                    Some(value) => Some(
                        value
                            .as_u64()
                            .ok_or_else(|| "'timeout_secs' must be an integer".to_string())?,
                    ),
                    None => None,
                };
                context.manager.wait(&ids, timeout)
            }),
            SubagentTool::Cancel => {
                string_array(args, "ids").and_then(|ids| context.manager.cancel(&ids, false))
            }
            SubagentTool::List => match args.get("id") {
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| "'id' must be a string".to_string())
                    .and_then(|id| context.manager.list(Some(id))),
                None => context.manager.list(None),
            },
        };
        match result {
            Ok(content) => ToolOutcome::ok(content),
            Err(error) => ToolOutcome::error(error),
        }
    }
}

const RESERVED_TOOL_NAMES: &[&str] = &[
    "subagent_spawn",
    "subagent_send",
    "subagent_wait",
    "subagent_cancel",
    "subagent_list",
];

fn subagent_tools(presets: &[AgentPreset]) -> Vec<ToolEntry> {
    let tool = |name: &str, description: &str, input_schema: Value, imp| ToolEntry {
        spec: ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema,
        },
        imp: ToolImpl::Subagent(imp),
    };
    let available_agents = presets
        .iter()
        .map(|preset| {
            let tools = preset
                .tools
                .as_ref()
                .map_or_else(|| "all tools".to_string(), |tools| tools.join("+"));
            format!("{} ({}): {}", preset.name, tools, preset.description)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let agent_names = presets
        .iter()
        .map(|preset| preset.name.clone())
        .collect::<Vec<_>>();
    let spawn_description = format!(
        "Start a self-contained background coding subagent and return its ID immediately. \
         Declare every tool the task needs in required_tools before choosing an agent. Omit agent \
         unless the selected preset includes every required tool. In particular, scout is only for \
         read-only inspection of existing files; any task that creates or modifies files must \
         require write_file or edit_file and use the default agent. Do not select a child model: \
         presets and user configuration may pin one, otherwise the active parent model is inherited. \
         Write the prompt as a contract: # Target (exact files and symbols, plus non-goals), # Change (steps), # Acceptance \
         (observable result). The agent must skip formatters, linters, and project-wide test suites. \
         Available agents: {available_agents}."
    );
    vec![
        tool(
            "subagent_spawn",
            &spawn_description,
            json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string"},
                    "required_tools": {
                        "type": "array",
                        "items": {"type": "string"},
                        "maxItems": 64,
                        "description": "Every tool the delegated task must use. Use [] only when the child can answer directly without tools. File creation requires write_file; file modification requires edit_file or write_file."
                    },
                    "name": {"type": "string"},
                    "agent": {
                        "type": "string",
                        "enum": agent_names,
                        "description": "Optional specialist preset. Omit this field for the default agent whenever the task needs a tool absent from the preset. Scout is read-only and cannot create or modify files."
                    }
                },
                "required": ["prompt", "required_tools"]
            }),
            SubagentTool::Spawn,
        ),
        tool(
            "subagent_send",
            "Queue another model-directed turn on a subagent, restarting it if settled.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "message": {"type": "string"}
                },
                "required": ["id", "message"]
            }),
            SubagentTool::Send,
        ),
        tool(
            "subagent_wait",
            "Wait until all requested subagents settle or the timeout expires without canceling them. \
             Every settled run reports its complete final response.",
            json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 64},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 300}
                },
                "required": ["ids"]
            }),
            SubagentTool::Wait,
        ),
        tool(
            "subagent_cancel",
            "Cancel active subagent runs, clear their queues, and retain partial transcripts.",
            json!({
                "type": "object",
                "properties": {
                    "ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 64}
                },
                "required": ["ids"]
            }),
            SubagentTool::Cancel,
        ),
        tool(
            "subagent_list",
            "List tracked subagents or return detailed status and the complete latest result for one ID.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}}
            }),
            SubagentTool::List,
        ),
    ]
}

fn validate_preset_capabilities(
    preset: &AgentPreset,
    required_tools: &[String],
) -> Result<(), String> {
    let Some(allowed_tools) = &preset.tools else {
        return Ok(());
    };
    let missing = required_tools
        .iter()
        .filter(|required| !allowed_tools.contains(required))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "agent '{}' does not provide required tool(s): {}; omit 'agent' to use the default agent or choose a compatible preset",
            preset.name,
            missing.join(", ")
        ))
    }
}

fn string_array(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let values = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing required array argument '{key}'"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("'{key}' must contain only strings"))
        })
        .collect()
}

fn truncate_result(content: &mut String) {
    if let Some((cut, _)) = content.char_indices().nth(MAX_RESULT_CHARS) {
        content.truncate(cut);
        content.push_str("\n[output truncated]");
    }
}

fn builtins() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            spec: ToolSpec {
                name: "shell".into(),
                description: "Run a shell command with `sh -c` in the current working directory. \
                              Returns stdout (and stderr / exit code on failure)."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The command to run"},
                        "timeout_secs": {"type": "integer", "description": "Optional timeout in seconds (default 120)"}
                    },
                    "required": ["command"]
                }),
            },
            imp: ToolImpl::Shell,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "read_file".into(),
                description: "Read a UTF-8 text file and return its contents.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path (absolute or relative to cwd)"}
                    },
                    "required": ["path"]
                }),
            },
            imp: ToolImpl::ReadFile,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "write_file".into(),
                description: "Write content to a file, creating parent directories as needed. \
                              Overwrites existing files."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            },
            imp: ToolImpl::WriteFile,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "edit_file".into(),
                description: "Replace an exact string in a file. `old_string` must appear exactly \
                              once; include enough surrounding context to make it unique."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "old_string": {"type": "string"},
                        "new_string": {"type": "string"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
            imp: ToolImpl::EditFile,
        },
    ]
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolOutcome> {
    args[key]
        .as_str()
        .ok_or_else(|| ToolOutcome::error(format!("missing required string argument '{key}'")))
}

fn shell(args: &Value) -> ToolOutcome {
    let command = match str_arg(args, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let timeout = Duration::from_secs(
        args["timeout_secs"]
            .as_u64()
            .unwrap_or(SHELL_DEFAULT_TIMEOUT_SECS),
    );
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    match exec::run_with_timeout(cmd, None, timeout) {
        Ok(result) => {
            let (content, is_error) = exec::render_result(&result, timeout);
            ToolOutcome { content, is_error }
        }
        Err(e) => ToolOutcome::error(format!("failed to spawn shell: {e}")),
    }
}

fn read_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::fs::File::open(path) {
        Ok(file) => read_bounded_utf8(path, file),
        Err(e) => ToolOutcome::error(format!("cannot read {path}: {e}")),
    }
}

fn read_bounded_utf8(path: &str, reader: impl Read) -> ToolOutcome {
    let mut bytes = Vec::new();
    if let Err(error) = reader
        .take(MAX_READ_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
    {
        return ToolOutcome::error(format!("cannot read {path}: {error}"));
    }
    if bytes.len() > MAX_READ_FILE_BYTES as usize {
        return ToolOutcome::error(format!(
            "{path} exceeds the {MAX_READ_FILE_BYTES}-byte read limit; \
             use shell tools to read portions"
        ));
    }
    match String::from_utf8(bytes) {
        Ok(text) => ToolOutcome::ok(text),
        Err(_) => ToolOutcome::error(format!("{path} is not valid UTF-8 (binary file?)")),
    }
}

fn write_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match str_arg(args, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolOutcome::error(format!("cannot create {}: {e}", parent.display()));
    }
    match std::fs::write(path, content) {
        Ok(()) => ToolOutcome::ok(format!("wrote {} bytes to {path}", content.len())),
        Err(e) => ToolOutcome::error(format!("cannot write {path}: {e}")),
    }
}

fn edit_file(args: &Value) -> ToolOutcome {
    let path = match str_arg(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old_string = match str_arg(args, "old_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let new_string = match str_arg(args, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    if old_string.is_empty() {
        return ToolOutcome::error("old_string must not be empty");
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return ToolOutcome::error(format!("cannot read {path}: {e}")),
    };
    let count = text.matches(old_string).count();
    match count {
        0 => ToolOutcome::error(format!("old_string not found in {path}")),
        1 => {
            let updated = text.replacen(old_string, new_string, 1);
            match std::fs::write(path, updated) {
                Ok(()) => ToolOutcome::ok(format!("edited {path}")),
                Err(e) => ToolOutcome::error(format!("cannot write {path}: {e}")),
            }
        }
        n => ToolOutcome::error(format!(
            "old_string appears {n} times in {path}; add surrounding context to make it unique"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("yawl-tools-{}-{name}", std::process::id()))
    }

    fn registry_config(home_dir: std::path::PathBuf, project_dir: std::path::PathBuf) -> Config {
        Config {
            model: Some("test".into()),
            max_tokens: 1,
            home_dir,
            project_dir,
            ..Config::test_default()
        }
    }

    #[test]
    fn edit_file_requires_unique_match() -> std::io::Result<()> {
        let path = temp_path("edit.txt");
        std::fs::write(&path, "aaa bbb aaa")?;
        let p = path.to_string_lossy();

        let dup = edit_file(&json!({"path": &p, "old_string": "aaa", "new_string": "x"}));
        assert!(dup.is_error);
        assert!(dup.content.contains("2 times"));

        let ok = edit_file(&json!({"path": &p, "old_string": "bbb", "new_string": "yyy"}));
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path)?, "aaa yyy aaa");
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn write_file_creates_parent_dirs() -> std::io::Result<()> {
        let dir = temp_path("nested");
        let file = dir.join("a/b.txt");
        let out = write_file(&json!({"path": file.to_string_lossy(), "content": "hi"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file)?, "hi");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn shell_builtin_reports_exit_code() {
        let out = shell(&json!({"command": "echo hello; exit 2"}));
        assert!(out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 2"));
    }

    #[test]
    fn registry_discovers_and_invokes_exec_tool() -> std::io::Result<()> {
        let root = temp_path("exec-registry");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;
        let tool_path = tools_dir.join("echo_session");
        std::fs::write(
            &tool_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  echo '{"name":"echo_session","description":"test tool","input_schema":{"type":"object"}}'
  exit 0
fi
input=$(cat)
printf '%s:%s' "$YAWL_SESSION_ID" "$input"
"#,
        )?;
        let mut permissions = std::fs::metadata(&tool_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions)?;

        let config = registry_config(home_dir, project_dir);
        let mut cache = DescribeCache::default();
        let registry = Registry::scan(&config, &mut cache);
        assert!(
            registry
                .describe_all()
                .iter()
                .any(|(name, _, _)| name == "echo_session")
        );
        let outcome = registry.execute("echo_session", r#"{"value":1}"#, "session-7");
        assert!(!outcome.is_error, "{}", outcome.content);
        assert_eq!(outcome.content, r#"session-7:{"value":1}"#);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn subagent_tools_are_conditional_and_reserved() -> std::io::Result<()> {
        let root = temp_path("reserved-subagent-tool");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir)?;
        let tool_path = tools_dir.join("reserved");
        std::fs::write(
            &tool_path,
            r#"#!/bin/sh
if [ "$1" = "--describe" ]; then
  echo '{"name":"subagent_spawn","description":"override","input_schema":{"type":"object"}}'
fi
"#,
        )?;
        let mut permissions = std::fs::metadata(&tool_path)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions)?;
        let mut config = registry_config(home_dir, project_dir);
        let mut cache = DescribeCache::default();

        let disabled = Registry::scan(&config, &mut cache);
        assert!(
            disabled
                .specs()
                .iter()
                .all(|spec| !spec.name.starts_with("subagent_"))
        );
        assert!(
            disabled
                .warnings
                .iter()
                .any(|warning| warning.contains("reserved"))
        );

        config.subagents = true;
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let enabled = Registry::scan_with_subagents(&config, &mut cache, manager, "test");
        let names = enabled
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert!(
            RESERVED_TOOL_NAMES
                .iter()
                .all(|name| names.iter().any(|candidate| candidate == name))
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn spawn_tool_lists_presets_and_takes_an_optional_agent() {
        let root = temp_path("preset-spawn-tool");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry =
            Registry::scan_with_subagents(&config, &mut DescribeCache::default(), manager, "test");

        let spawn = registry
            .specs()
            .into_iter()
            .find(|spec| spec.name == "subagent_spawn")
            .expect("spawn tool present");
        assert!(
            spawn.description.contains("scout (read_file)"),
            "the description should advertise bundled presets; got:\n{}",
            spawn.description
        );
        assert!(spawn.description.contains("# Target"));
        let required = spawn.input_schema.get("required").expect("required list");
        assert_eq!(
            required,
            &json!(["prompt", "required_tools"]),
            "name and agent are optional, but capability planning is required"
        );
        assert!(
            spawn
                .description
                .contains("any task that creates or modifies files")
        );
        let properties = spawn
            .input_schema
            .get("properties")
            .expect("properties object");
        assert!(properties.get("agent").is_some());
        assert!(properties.get("required_tools").is_some());
        assert!(
            properties.get("model").is_none(),
            "the orchestrator must not override the configured child model"
        );
        assert!(
            properties["agent"]["description"]
                .as_str()
                .is_some_and(|text| text.contains("Scout is read-only"))
        );
    }

    #[test]
    fn spawn_without_model_inherits_the_active_parent() {
        let root = temp_path("spawn-model-inherit");
        let mut config = registry_config(root.join("home"), root.join("project"));
        config.openai_base_url = "http://127.0.0.1:9/v1".into();
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "openai:active-parent",
        );
        let args = json!({
            "prompt": "Report the delegated result directly.",
            "required_tools": []
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(!outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert_eq!(manager.snapshots()[0].model, "openai:active-parent");
        manager.shutdown_and_discard();
    }

    #[test]
    fn spawn_rejects_model_overrides_from_the_orchestrator() {
        let root = temp_path("spawn-model-override");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "openai-codex:parent",
        );
        let args = json!({
            "prompt": "Inspect the requested file.",
            "required_tools": ["read_file"],
            "model": "gpt-4.1-mini"
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert!(
            outcome.content.contains("not accepted"),
            "{}",
            outcome.content
        );
        assert!(manager.snapshots().is_empty());
        manager.shutdown_and_discard();
    }

    #[test]
    fn scout_rejects_spawn_tasks_that_require_file_writes() {
        let root = temp_path("scout-write-capability");
        let config = registry_config(root.join("home"), root.join("project"));
        let manager = SubagentManager::new("session".into(), config.max_subagents);
        let registry = Registry::scan_with_subagents(
            &config,
            &mut DescribeCache::default(),
            manager.clone(),
            "test",
        );
        let args = json!({
            "agent": "scout",
            "prompt": "Create short_story.txt and write a story into it.",
            "required_tools": ["write_file"]
        });

        let outcome = registry.execute("subagent_spawn", &args.to_string(), "session");

        assert!(outcome.is_error, "unexpected outcome: {}", outcome.content);
        assert!(outcome.content.contains("scout"), "{}", outcome.content);
        assert!(
            outcome.content.contains("write_file"),
            "{}",
            outcome.content
        );
        manager.shutdown_and_discard();
    }

    #[test]
    fn retain_names_filters_the_registry_for_preset_children() {
        let root = temp_path("preset-allowlist");
        let config = registry_config(root.join("home"), root.join("project"));
        let mut registry = Registry::scan(&config, &mut DescribeCache::default());

        registry.retain_names(&["read_file".to_string(), "shell".to_string()]);

        let mut names = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, ["read_file", "shell"]);
    }

    #[test]
    fn read_file_rejects_files_over_the_size_limit() -> std::io::Result<()> {
        let path = temp_path("large.bin");
        std::fs::write(&path, vec![b'a'; MAX_READ_FILE_BYTES as usize + 1])?;

        let out = read_file(&json!({"path": path.to_string_lossy()}));

        assert!(out.is_error);
        assert!(out.content.contains("read limit"));
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn read_file_bounds_streams_without_relying_on_metadata() {
        let out = read_bounded_utf8("endless", std::io::repeat(b'a'));

        assert!(out.is_error);
        assert!(out.content.contains("read limit"));
    }

    #[test]
    fn result_truncation_preserves_utf8_boundaries() {
        let exact = "é".repeat(MAX_RESULT_CHARS);
        let mut unchanged = exact.clone();
        truncate_result(&mut unchanged);
        assert_eq!(unchanged, exact);

        let mut truncated = "é".repeat(MAX_RESULT_CHARS + 1);
        truncate_result(&mut truncated);
        assert!(truncated.ends_with("\n[output truncated]"));
        assert_eq!(truncated.matches('é').count(), MAX_RESULT_CHARS);
    }
}
