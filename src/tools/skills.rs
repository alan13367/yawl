//! The `read_skill` tool: loads an advertised skill's full instructions.

use serde_json::{Value, json};

use super::{ToolEntry, ToolImpl, ToolOutcome, str_arg};
use crate::provider::{Message, ToolSpec};
use crate::skills::Skill;

pub(super) fn entry() -> ToolEntry {
    ToolEntry::new(
        ToolSpec {
            name: "read_skill".into(),
            description: "Load an advertised skill's full instructions. Call this before acting on any task a skill's description matches: descriptions only say when a skill applies, and the commands, hosts, credentials, and paths the task needs are in the instructions.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Exact advertised skill name"}
                },
                "required": ["name"]
            }),
        },
        ToolImpl::ReadSkill,
    )
}

/// A short reply when the conversation still holds this skill's exact text.
/// Checking the history rather than remembering what was loaded means a skill
/// that compaction removed, or whose file changed, loads again on its own.
pub(super) fn already_loaded(
    skills: &[Skill],
    args: &Value,
    history: &[Message],
) -> Option<ToolOutcome> {
    let name = args.get("name")?.as_str()?;
    let skill = skills.iter().find(|skill| skill.name == name)?;
    let text = crate::skills::tool_result(skill);
    history
        .iter()
        .any(|message| {
            message.tool_name.as_deref() == Some("read_skill")
                && !message.is_error
                && message.content == text
        })
        .then(|| {
            ToolOutcome::ok(format!(
                "Skill {name} is already loaded earlier in this conversation and has not changed. Follow those instructions."
            ))
        })
}

pub(super) fn read(skills: &[Skill], args: &Value) -> ToolOutcome {
    let name = match str_arg(args, "name") {
        Ok(name) => name,
        Err(error) => return error,
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        return ToolOutcome::error(format!(
            "skill '{name}' is not available for model invocation"
        ));
    };
    ToolOutcome::ok(crate::skills::tool_result(skill))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(instructions: &str) -> Skill {
        Skill {
            name: "demo".into(),
            description: "Demo".into(),
            directory: "/skills/demo".into(),
            instructions: instructions.into(),
            disable_model_invocation: false,
        }
    }

    fn loaded(content: String, is_error: bool) -> Message {
        Message::tool_result("call", "read_skill", content, is_error)
    }

    #[test]
    fn a_skill_still_in_the_conversation_is_not_sent_twice() {
        let skills = [skill("Do the thing.")];
        let args = json!({"name": "demo"});
        let text = crate::skills::tool_result(&skills[0]);
        assert!(already_loaded(&skills, &args, &[]).is_none());

        let note =
            already_loaded(&skills, &args, &[loaded(text.clone(), false)]).expect("already loaded");
        assert!(!note.is_error);
        assert!(note.content.contains("already loaded"), "{}", note.content);

        // A failed read, other tools' output, and unknown skills do not count.
        assert!(already_loaded(&skills, &args, &[loaded(text.clone(), true)]).is_none());
        let other = Message::tool_result("call", "shell", text.clone(), false);
        assert!(already_loaded(&skills, &args, &[other]).is_none());
        let unknown = json!({"name": "other"});
        assert!(already_loaded(&skills, &unknown, &[loaded(text, false)]).is_none());
    }

    #[test]
    fn a_skill_whose_file_changed_loads_again() {
        let old = crate::skills::tool_result(&skill("Old instructions."));
        let skills = [skill("New instructions.")];
        assert!(already_loaded(&skills, &json!({"name": "demo"}), &[loaded(old, false)]).is_none());
    }
}
