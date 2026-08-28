//! Compact coding-agent system prompt plus optional global and project
//! instructions from `AGENTS.md` files.

use std::path::Path;

use crate::skills::Skill;

pub(crate) fn build_system_prompt(
    global_dir: &Path,
    subagents: bool,
    print_mode: bool,
    skills: &[Skill],
) -> String {
    let cwd = std::env::current_dir().ok();
    build_system_prompt_from(
        cwd.as_deref(),
        global_dir,
        subagents,
        false,
        print_mode,
        None,
        skills,
    )
}

pub(crate) fn build_subagent_system_prompt(
    global_dir: &Path,
    role_fragment: Option<&str>,
    skills: &[Skill],
) -> String {
    let cwd = std::env::current_dir().ok();
    build_system_prompt_from(
        cwd.as_deref(),
        global_dir,
        false,
        true,
        false,
        role_fragment,
        skills,
    )
}

fn build_system_prompt_from(
    cwd: Option<&Path>,
    global_dir: &Path,
    subagents: bool,
    is_subagent: bool,
    print_mode: bool,
    role_fragment: Option<&str>,
    skills: &[Skill],
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
- Yawl rescans executable tools in `~/.yawl/tools/` and `./.yawl/tools/` before each model step. Add one with an executable whose `--describe` returns JSON fields `name`, `description`, `input_schema`, and optional `timeout_secs`. Calls read JSON stdin, write stdout, run in the working directory with `YAWL_SESSION_ID`, and report errors with a nonzero exit.
"#
    );
    if !is_subagent {
        prompt.push_str(
            "- Run long commands with shell background=true; use shell_output, shell_list, and shell_stop with the returned bg-N ID.\n",
        );
    }
    append_skill_catalog(&mut prompt, skills);
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
            "- Print mode delivers settled results after the turn; wait only when blocked.\n"
        } else {
            "- TUI results arrive automatically; wait only when blocked.\n"
        };
        prompt.push_str(r#"
<subagent_guidance>
- Before spawning, inspect the repository root, top-level files, instructions, and empty or uninitialized state yourself; never use scout for discovery.
- Delegate only useful, self-contained work. Prompts need # Target (paths, ownership, non-goals), # Change, and # Acceptance. Give parallel agents disjoint scopes, define interfaces first, and keep working.
- Declare every required tool; use [] only for tool-free answers. Omit agent for commands, file changes, or missing preset capabilities. File changes require write_file or edit_file. scout may only read named files or skills. Never set a model.
- Children lack this conversation and cannot delegate. Use subagent_send for follow-ups. They skip project-wide formatting, linting, builds, and tests; validate once after all finish. Large disk output requires the default agent and write_file.
- subagent_wait without a timeout blocks until every ID settles. Set timeout_secs only for a bounded status check. Cancel only unwanted work.
"#);
        prompt.push_str(delivery);
        prompt.push_str("</subagent_guidance>\n");
    }
    if is_subagent {
        prompt.push_str(
            "\n<subagent_role>\nYou are a background subagent working on one delegated task. You share the parent's working directory but not its conversation. Do not ask the user questions and do not create subagents. Preserve concurrent edits and do not revert work you did not make. Project-wide validation is the parent's job: never run formatters, linters, or project-wide builds or test suites unless your task explicitly requires it; scoped proof of your own change is fine. Complete only the delegated task, verify it when possible, and return a concise result to the parent.\n",
        );
        if let Some(fragment) = role_fragment.map(str::trim)
            && !fragment.is_empty()
        {
            prompt.push_str(fragment);
            prompt.push('\n');
        }
        prompt.push_str("</subagent_role>\n");
    }
    prompt
}

fn append_skill_catalog(prompt: &mut String, skills: &[Skill]) {
    if skills.is_empty() {
        return;
    }
    prompt.push_str(
        "\n<skills>\nReusable skills, listed as name and description. Check this catalog for every request. When a description matches the task, call read_skill with the skill name and follow the returned instructions before acting. The description only says when a skill applies; never apply a skill from its description alone.\n",
    );
    for skill in skills {
        prompt.push_str("<skill name=\"");
        push_xml_escaped(prompt, &skill.name);
        prompt.push_str("\">");
        push_xml_escaped(prompt, &skill.description);
        prompt.push_str("</skill>\n");
    }
    prompt.push_str("</skills>\n");
}

fn push_xml_escaped(output: &mut String, value: &str) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            _ => output.push(character),
        }
    }
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
        let prompt = build_system_prompt_from(
            Some(&dirs.0),
            &dirs.0.join("global"),
            false,
            false,
            false,
            None,
            &[],
        );
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

        let prompt = build_system_prompt_from(
            Some(&project_dir),
            &global_dir,
            false,
            false,
            false,
            None,
            &[],
        );
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
        let main = build_system_prompt_from(Some(&dirs.0), &dirs.0, true, false, false, None, &[]);
        let child = build_system_prompt_from(Some(&dirs.0), &dirs.0, false, true, false, None, &[]);
        let disabled =
            build_system_prompt_from(Some(&dirs.0), &dirs.0, false, false, false, None, &[]);
        let print = build_system_prompt_from(Some(&dirs.0), &dirs.0, true, false, true, None, &[]);

        assert!(main.contains("<subagent_guidance>"));
        assert!(child.contains("<subagent_role>"));
        assert!(!child.contains("<subagent_guidance>"));
        assert!(!disabled.contains("subagent_guidance"));
        assert!(
            main.len() < 2_500,
            "orchestration guidance should stay compact; got {} bytes",
            main.len()
        );
        assert!(print.contains("Print mode delivers settled results after the turn"));
        assert!(
            main.contains("# Target") && main.contains("# Change") && main.contains("# Acceptance"),
            "the spawn prompt contract must be part of the guidance"
        );
        assert!(
            main.contains("skip project-wide formatting, linting, builds, and tests"),
            "the mid-flight validation ban must reach the main agent"
        );
        assert!(
            main.contains("subagent_wait without a timeout blocks until every ID settles")
                && main.contains("Cancel only unwanted work"),
            "the guidance must describe blocking waits and forbid canceling useful work"
        );
        assert!(
            main.contains("Declare every required tool")
                && main.contains("Never set a model")
                && main.contains("scout may only read named files or skills")
                && main.contains("Omit agent for commands, file changes"),
            "the parent must route write tasks away from read-only presets"
        );
        assert!(
            child.contains("Project-wide validation is the parent's job"),
            "the mid-flight validation ban must reach the child"
        );
    }

    #[test]
    fn subagent_guidance_requires_a_local_repository_check_before_delegation() {
        let dirs = TestDirs::new();
        let prompt =
            build_system_prompt_from(Some(&dirs.0), &dirs.0, true, false, false, None, &[]);
        let local_check = prompt
            .find("Before spawning")
            .expect("the parent must check the working directory before spawning");
        let delegation = prompt
            .find("Delegate only useful, self-contained work")
            .expect("the delegation guidance should be present");

        assert!(local_check < delegation);
        assert!(prompt.contains("empty or uninitialized state yourself"));
        assert!(prompt.contains("never use scout for discovery"));
    }

    #[test]
    fn role_fragment_is_appended_inside_the_role_block() {
        let dirs = TestDirs::new();
        let child = build_system_prompt_from(
            Some(&dirs.0),
            &dirs.0,
            false,
            true,
            false,
            Some("You are a scout: investigate and report paths."),
            &[],
        );

        let block_start = child.find("<subagent_role>").expect("role block opens");
        let block_end = child.find("</subagent_role>").expect("role block closes");
        let fragment_at = child
            .find("You are a scout: investigate and report paths.")
            .expect("fragment is present");
        assert!(
            block_start < fragment_at && fragment_at < block_end,
            "the preset fragment must land inside the role block"
        );
        let plain = build_subagent_system_prompt(&dirs.0, None, &[]);
        assert!(!plain.contains("You are a scout"));
    }

    #[test]
    fn skill_catalog_keeps_complete_descriptions_and_omits_paths() {
        let dirs = TestDirs::new();
        let long = format!("Choose this for <reviews> & fixes. {}", "x".repeat(4_000));
        let skill = Skill {
            name: "review".into(),
            description: long.clone(),
            path: dirs.0.join("secret/SKILL.md"),
            directory: dirs.0.join("secret"),
            instructions: "instructions are loaded later".into(),
            disable_model_invocation: false,
        };

        let prompt =
            build_system_prompt_from(Some(&dirs.0), &dirs.0, false, false, false, None, &[skill]);
        assert!(
            prompt.contains(
                &long
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            )
        );
        assert!(prompt.contains("call read_skill with the skill name"));
        assert!(prompt.contains("never apply a skill from its description alone"));
        assert!(!prompt.contains("secret/SKILL.md"));
        assert!(!prompt.contains("instructions are loaded later"));
    }
}
