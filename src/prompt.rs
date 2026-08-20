//! Compact coding-agent system prompt plus optional project instructions.

pub fn build_system_prompt() -> String {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "(unknown)".to_string());
    let mut prompt = format!(
        r#"You are Yawl, an expert coding agent. Help the user by inspecting repositories, running commands, editing code, and creating files.

Current working directory: {cwd}

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
    if let Ok(project) = std::fs::read_to_string("YAWL.md") {
        let trimmed = project.trim();
        if !trimmed.is_empty() {
            prompt.push_str("\n<project_instructions path=\"YAWL.md\">\n");
            prompt.push_str(trimmed);
            prompt.push_str("\n</project_instructions>\n");
        }
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_coding_focused_and_documents_extension_contract() {
        let prompt = build_system_prompt();
        assert!(prompt.contains("expert coding agent"));
        assert!(prompt.contains("--describe"));
        assert!(prompt.contains("YAWL_SESSION_ID"));
        assert!(prompt.len() < 2_500);
    }
}
