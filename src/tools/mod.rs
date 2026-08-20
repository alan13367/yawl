//! Tool registry: builtins plus exec tools discovered from disk.
//!
//! The registry is rescanned every agent-loop iteration, so a tool the model
//! just wrote is usable on its next turn. On name collisions the last scan
//! wins (builtins < `~/.yawl/tools` < `./.yawl/tools`).

pub mod exec;

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::provider::ToolSpec;

pub use exec::DescribeCache;

/// Cap on tool result size fed back to the model.
const MAX_RESULT_CHARS: usize = 60_000;
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 120;

enum ToolImpl {
    Shell,
    ReadFile,
    WriteFile,
    EditFile,
    Exec(exec::ExecTool),
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
}

impl Registry {
    /// Scans builtins + exec tool directories. Called every loop iteration;
    /// `cache` avoids respawning `--describe` for unchanged tools.
    pub fn scan(config: &Config, cache: &mut DescribeCache) -> Registry {
        let mut registry = Registry {
            entries: builtins(),
            warnings: Vec::new(),
        };
        for dir in config.tool_dirs() {
            let (tools, warnings) = exec::scan_dir(&dir, cache);
            registry.warnings.extend(warnings);
            for tool in tools {
                registry.insert(ToolEntry {
                    spec: tool.spec.clone(),
                    imp: ToolImpl::Exec(tool),
                });
            }
        }
        registry
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
        };
        if outcome.content.chars().count() > MAX_RESULT_CHARS {
            let cut: String = outcome.content.chars().take(MAX_RESULT_CHARS).collect();
            outcome.content = format!("{cut}\n[output truncated]");
        }
        if outcome.content.is_empty() {
            outcome.content = if outcome.is_error {
                "(no output)".to_string()
            } else {
                "(no output; command succeeded)".to_string()
            };
        }
        outcome
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
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => ToolOutcome::ok(text),
            Err(_) => ToolOutcome::error(format!("{path} is not valid UTF-8 (binary file?)")),
        },
        Err(e) => ToolOutcome::error(format!("cannot read {path}: {e}")),
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

    #[test]
    fn edit_file_requires_unique_match() {
        let path = temp_path("edit.txt");
        std::fs::write(&path, "aaa bbb aaa").unwrap();
        let p = path.to_str().unwrap();

        let dup = edit_file(&json!({"path": p, "old_string": "aaa", "new_string": "x"}));
        assert!(dup.is_error);
        assert!(dup.content.contains("2 times"));

        let ok = edit_file(&json!({"path": p, "old_string": "bbb", "new_string": "yyy"}));
        assert!(!ok.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aaa yyy aaa");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let dir = temp_path("nested");
        let file = dir.join("a/b.txt");
        let out = write_file(&json!({"path": file.to_str().unwrap(), "content": "hi"}));
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_builtin_reports_exit_code() {
        let out = shell(&json!({"command": "echo hello; exit 2"}));
        assert!(out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 2"));
    }

    #[test]
    fn registry_discovers_and_invokes_exec_tool() {
        let root = temp_path("exec-registry");
        let home_dir = root.join("home");
        let project_dir = root.join("project");
        let tools_dir = project_dir.join("tools");
        std::fs::create_dir_all(&tools_dir).unwrap();
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
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&tool_path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&tool_path, permissions).unwrap();

        let config = Config {
            model: Some("test".into()),
            anthropic_base_url: String::new(),
            openai_base_url: String::new(),
            max_tokens: 1,
            reasoning_effort: None,
            hide_reasoning: false,
            context_windows: std::collections::HashMap::new(),
            auto_compact: true,
            compact_threshold: 0.85,
            skill_dirs: Vec::new(),
            providers: std::collections::HashMap::new(),
            home_dir,
            project_dir,
        };
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
    }
}
