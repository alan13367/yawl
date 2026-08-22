//! Compact coding-agent system prompt plus optional global and project
//! instructions from `AGENTS.md` files.

use std::path::Path;

pub(crate) fn build_system_prompt(global_dir: &Path, subagents: bool, print_mode: bool) -> String {
    let cwd = std::env::current_dir().ok();
    build_system_prompt_from(cwd.as_deref(), global_dir, subagents, false, print_mode)
}

pub(crate) fn build_subagent_system_prompt(global_dir: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    build_system_prompt_from(cwd.as_deref(), global_dir, false, true, false)
}

fn build_system_prompt_from(
    cwd: Option<&Path>,
    global_dir: &Path,
    subagents: bool,
    is_subagent: bool,
    print_mode: bool,
) -> String {
    let cwd_display = cwd.map_or_else(
        || "(unknown)".to_string(),
        |path| path.display().to_string(),
    );
    let mut prompt = format!(
        r#"You are Yawl, an expert coding agent. Help the user by inspecting repositories, running commands, editing code, and creating files.

Current working directory: {cwd_display}

Guidelines:
- Read the relevant code and project instructions before editing.
- Use tools instead of guessing. Preserve unrelated user changes.
- Make focused, complete changes and verify them with the repository's tests or checks.
- Report outcomes and file paths clearly. Keep responses concise.

Tools:
- Builtins: shell, read_file, write_file, edit_file.
- Yawl also loads executable tools from `~/.yawl/tools/` and `./.yawl/tools/` before every model step.
- To add a tool, create an executable whose `--describe` output is JSON with `name`, `description`, `input_schema`, and optional `timeout_secs`. Normal calls receive JSON on stdin and return their result on stdout. A nonzero exit is an error. The tool inherits the working directory and receives `YAWL_SESSION_ID`.
"#
    );
    append_instructions(
        &mut prompt,
        "global_instructions",
        "~/.yawl/AGENTS.md",
        &global_dir.join("AGENTS.md"),
    );
    if let Some(cwd) = cwd {
        append_instructions(
            &mut prompt,
            "project_instructions",
            "AGENTS.md",
            &cwd.join("AGENTS.md"),
        );
    }
    if subagents {
        let delivery = if print_mode {
            "- Print mode has no automatic follow-up. Wait for every spawned child before finishing.\n"
        } else {
            "- TUI results arrive automatically. Wait only when your next step depends on a result.\n"
        };
        prompt.push_str(r#"
<subagent_guidance>
- Delegate only self-contained work. Include paths, constraints, file ownership, and expected output.
- Give concurrent agents disjoint editing scopes. Spawn them in the background and keep working.
"#);
        prompt.push_str(delivery);
        prompt.push_str(
            "- Use subagent_send for more model-directed work. Children do not see this conversation and cannot create subagents.\n</subagent_guidance>\n",
        );
    }
    if is_subagent {
        prompt.push_str(
            r#"
<subagent_role>
You are a background subagent working on one delegated task. You share the parent's working directory but not its conversation. Do not ask the user questions and do not create subagents. Preserve concurrent edits and do not revert work you did not make. Complete only the delegated task, verify it when possible, and return a concise result to the parent.
</subagent_role>
"#,
        );
    }
    prompt
}

fn append_instructions(prompt: &mut String, tag: &str, display_path: &str, path: &Path) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let instructions = contents.trim();
    if instructions.is_empty() {
        return;
    }
    prompt.push_str("\n<");
    prompt.push_str(tag);
    prompt.push_str(" path=\"");
    prompt.push_str(display_path);
    prompt.push_str("\">\n");
    prompt.push_str(instructions);
    prompt.push_str("\n</");
    prompt.push_str(tag);
    prompt.push_str(">\n");
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirs(PathBuf);

    impl TestDirs {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            Self(std::env::temp_dir().join(format!("yawl-prompt-{}-{nonce}", std::process::id())))
        }
    }

    impl Drop for TestDirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prompt_is_coding_focused_and_documents_extension_contract() {
        let dirs = TestDirs::new();
        let prompt =
            build_system_prompt_from(Some(&dirs.0), &dirs.0.join("global"), false, false, false);
        assert!(prompt.contains("expert coding agent"));
        assert!(prompt.contains("--describe"));
        assert!(prompt.contains("YAWL_SESSION_ID"));
        assert!(prompt.len() < 2_500);
    }

    #[test]
    fn global_and_project_agents_instructions_are_injected_in_order() {
        let dirs = TestDirs::new();
        let global_dir = dirs.0.join("global");
        let project_dir = dirs.0.join("project");
        std::fs::create_dir_all(&global_dir).expect("global test directory should be created");
        std::fs::create_dir_all(&project_dir).expect("project test directory should be created");
        std::fs::write(global_dir.join("AGENTS.md"), "global rule")
            .expect("global instructions should be written");
        std::fs::write(project_dir.join("AGENTS.md"), "project rule")
            .expect("project instructions should be written");
        std::fs::write(project_dir.join("YAWL.md"), "legacy rule")
            .expect("legacy instructions should be written");

        let prompt = build_system_prompt_from(Some(&project_dir), &global_dir, false, false, false);
        let global_position = prompt
            .find("global rule")
            .expect("global instructions should be present");
        let project_position = prompt
            .find("project rule")
            .expect("project instructions should be present");

        assert!(global_position < project_position);
        assert!(prompt.contains("<global_instructions path=\"~/.yawl/AGENTS.md\">"));
        assert!(prompt.contains("<project_instructions path=\"AGENTS.md\">"));
        assert!(!prompt.contains("legacy rule"));
    }

    #[test]
    fn orchestration_and_subagent_guidance_are_conditional() {
        let dirs = TestDirs::new();
        let main = build_system_prompt_from(Some(&dirs.0), &dirs.0, true, false, false);
        let child = build_system_prompt_from(Some(&dirs.0), &dirs.0, false, true, false);
        let disabled = build_system_prompt_from(Some(&dirs.0), &dirs.0, false, false, false);
        let print = build_system_prompt_from(Some(&dirs.0), &dirs.0, true, false, true);

        assert!(main.contains("<subagent_guidance>"));
        assert!(child.contains("<subagent_role>"));
        assert!(!child.contains("<subagent_guidance>"));
        assert!(!disabled.contains("subagent_guidance"));
        assert!(print.contains("Wait for every spawned child"));
    }
}
