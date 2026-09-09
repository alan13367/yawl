//! Foreground `sh -c` execution and background process tools.
//!
//! The registry owns discovery and dispatch; this module owns the shell
//! schemas and their execution against [`BackgroundProcessManager`].

use std::time::Duration;

use serde_json::{Value, json};

use crate::background::{BackgroundProcessManager, OutputRead, StartSpec};
use crate::provider::ToolSpec;

use super::{ToolEntry, ToolImpl, ToolOutcome, str_arg};

const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 120;

pub(super) fn entry(background: bool) -> ToolEntry {
    let mut properties = json!({
        "command": {"type": "string"},
        "timeout_secs": {"type": "integer", "minimum": 1, "description": "Seconds; foreground defaults to 120, background to unlimited."}
    });
    let description = if background {
        properties["background"] = json!({
            "type": "boolean",
            "description": "Run in background and return a bg-N ID"
        });
        properties["name"] = json!({
            "type": "string",
            "maxLength": 80,
            "description": "Optional /ps label"
        });
        "Run `sh -c` in the working directory. Use background=true for long commands, then shell_output, shell_list, or shell_stop."
    } else {
        "Run foreground `sh -c` in the working directory; return stdout or the failure."
    };
    ToolEntry {
        spec: ToolSpec {
            name: "shell".into(),
            description: description.into(),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": ["command"]
            }),
        },
        imp: ToolImpl::Shell,
    }
}

pub(super) fn background_entries() -> Vec<ToolEntry> {
    vec![
        ToolEntry {
            spec: ToolSpec {
                name: "shell_list".into(),
                description: "List background commands with ID, status, PID, elapsed time, label, and command.".into(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
            imp: ToolImpl::ShellList,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "shell_output".into(),
                description: "Read new background-command output. Reuse next_cursor; wait_secs may wait for output or completion.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "bg-N ID"},
                        "cursor": {"type": "integer", "minimum": 0, "description": "Previous cursor; default 0"},
                        "wait_secs": {"type": "integer", "minimum": 0, "maximum": 30, "description": "Wait seconds; default 0"}
                    },
                    "required": ["id"]
                }),
            },
            imp: ToolImpl::ShellOutput,
        },
        ToolEntry {
            spec: ToolSpec {
                name: "shell_stop".into(),
                description: "Gracefully stop a background process group; settled commands return status.".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "bg-N ID"}},
                    "required": ["id"]
                }),
            },
            imp: ToolImpl::ShellStop,
        },
    ]
}

pub(super) fn execute(args: &Value, background: Option<&BackgroundProcessManager>) -> ToolOutcome {
    execute_with_path(args, background, None)
}

pub(super) fn execute_with_path(
    args: &Value,
    background: Option<&BackgroundProcessManager>,
    path: Option<&std::ffi::OsStr>,
) -> ToolOutcome {
    let command = match str_arg(args, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let run_in_background = match args.get("background") {
        Some(Value::Bool(value)) => *value,
        None => false,
        Some(_) => return ToolOutcome::error("'background' must be a boolean when provided"),
    };
    if run_in_background {
        let Some(background) = background else {
            return ToolOutcome::error("background shell execution is not available");
        };
        let name = match args.get("name") {
            Some(Value::String(name)) if name.trim().chars().count() > 80 => {
                return ToolOutcome::error("'name' must be at most 80 characters");
            }
            Some(Value::String(name)) if !name.trim().is_empty() => Some(name.trim().to_string()),
            Some(Value::String(_)) | None => None,
            Some(_) => return ToolOutcome::error("'name' must be a string when provided"),
        };
        let timeout = match args.get("timeout_secs") {
            Some(value) => match value.as_u64() {
                Some(0) | None => {
                    return ToolOutcome::error("'timeout_secs' must be a positive integer");
                }
                Some(seconds) => Some(Duration::from_secs(seconds)),
            },
            None => None,
        };
        return match background.start(StartSpec {
            command: command.to_string(),
            name: name.clone(),
            cwd: crate::config::working_dir(),
            timeout,
        }) {
            Ok(started) => ToolOutcome::ok(format!(
                "started {} (pid {}){} in the background\nnext_cursor: 0",
                started.id,
                started.pid,
                name.map_or_else(String::new, |name| format!(" as {name}"))
            )),
            Err(error) => ToolOutcome::error(error),
        };
    }
    let timeout_secs = match args.get("timeout_secs") {
        Some(value) => match value.as_u64() {
            Some(0) | None => {
                return ToolOutcome::error("'timeout_secs' must be a positive integer");
            }
            Some(seconds) => seconds,
        },
        None => SHELL_DEFAULT_TIMEOUT_SECS,
    };
    let timeout = Duration::from_secs(timeout_secs);
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    match super::exec::run_with_timeout(cmd, None, timeout) {
        Ok(result) => {
            let (content, is_error) = super::exec::render_result(&result, timeout);
            ToolOutcome {
                content,
                images: Vec::new(),
                is_error,
            }
        }
        Err(e) => ToolOutcome::error(format!("failed to spawn shell: {e}")),
    }
}

pub(super) fn list(background: Option<&BackgroundProcessManager>) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let snapshots = background.snapshots();
    if snapshots.is_empty() {
        return ToolOutcome::ok("no background shell commands are tracked".into());
    }
    let now = std::time::Instant::now();
    let mut output = String::new();
    for snapshot in snapshots {
        let pid = snapshot
            .pid
            .map_or_else(|| "?".into(), |pid| pid.to_string());
        output.push_str(&format!(
            "{}  {}  pid={}  elapsed={}s  {}  command={}\n",
            snapshot.id,
            snapshot.status.detail(),
            pid,
            snapshot.elapsed(now).as_secs(),
            snapshot.name.as_deref().unwrap_or("unnamed"),
            snapshot.command
        ));
    }
    ToolOutcome::ok(output.trim_end().to_string())
}

pub(super) fn output(background: Option<&BackgroundProcessManager>, args: &Value) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let id = match str_arg(args, "id") {
        Ok(id) => id,
        Err(error) => return error,
    };
    let cursor = match args.get("cursor") {
        Some(value) => match value.as_u64() {
            Some(cursor) => cursor,
            None => return ToolOutcome::error("'cursor' must be a non-negative integer"),
        },
        None => 0,
    };
    let wait_secs = match args.get("wait_secs") {
        Some(value) => match value.as_u64() {
            Some(wait) => wait,
            None => return ToolOutcome::error("'wait_secs' must be a non-negative integer"),
        },
        None => 0,
    };
    if wait_secs > 30 {
        return ToolOutcome::error("'wait_secs' must be between 0 and 30");
    }
    match background.read_output(id, cursor, Duration::from_secs(wait_secs)) {
        Ok(read) => ToolOutcome::ok(format_output(read)),
        Err(error) => ToolOutcome::error(error),
    }
}

pub(super) fn format_output(read: OutputRead) -> String {
    let mut output = format!(
        "{}: {} (pid {})\n",
        read.snapshot.id,
        read.snapshot.status.detail(),
        read.snapshot
            .pid
            .map_or_else(|| "?".into(), |pid| pid.to_string())
    );
    if read.stale_cursor {
        output.push_str("[earlier output was discarded]\n");
    }
    let mut previous = None;
    for chunk in read.chunks {
        if previous != Some(chunk.stream) {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(&format!("[{}]\n", chunk.stream.label()));
            previous = Some(chunk.stream);
        }
        output.push_str(&chunk.text);
    }
    if !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!("next_cursor: {}", read.next_cursor));
    output
}

pub(super) fn stop(background: Option<&BackgroundProcessManager>, args: &Value) -> ToolOutcome {
    let Some(background) = background else {
        return ToolOutcome::error("background shell execution is not available");
    };
    let id = match str_arg(args, "id") {
        Ok(id) => id,
        Err(error) => return error,
    };
    match background.stop(id) {
        Ok(snapshot) if snapshot.status.is_active() => {
            ToolOutcome::ok(format!("stop requested for {id}"))
        }
        Ok(snapshot) => ToolOutcome::ok(format!("{id}: {}", snapshot.status.detail())),
        Err(error) => ToolOutcome::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::background::{
        BackgroundId, BackgroundSnapshot, BackgroundStatus, LogChunk, OutputStream,
    };

    #[test]
    fn shell_builtin_reports_exit_code() {
        let out = execute(&json!({"command": "echo hello; exit 2"}), None);
        assert!(out.is_error);
        assert!(out.content.contains("hello"));
        assert!(out.content.contains("exit code: 2"));
    }

    #[test]
    fn background_output_keeps_adjacent_read_chunks_contiguous() {
        let now = std::time::Instant::now();
        let output = format_output(OutputRead {
            snapshot: BackgroundSnapshot {
                id: BackgroundId::new(1),
                pid: Some(42),
                command: "printf hello".into(),
                name: None,
                cwd: std::path::PathBuf::from("."),
                timeout: None,
                status: BackgroundStatus::Running,
                started_at: now,
                settled_at: None,
            },
            chunks: vec![
                LogChunk {
                    cursor: 0,
                    stream: OutputStream::Stdout,
                    text: "hello ".into(),
                },
                LogChunk {
                    cursor: 1,
                    stream: OutputStream::Stdout,
                    text: "world".into(),
                },
            ],
            stale_cursor: false,
            next_cursor: 2,
        });

        assert!(output.contains("[stdout]\nhello world\nnext_cursor: 2"));
        assert!(!output.contains("hello \nworld"));
    }
}
