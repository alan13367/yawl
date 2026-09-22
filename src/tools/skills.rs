//! The `read_skill` tool: loads an advertised skill's full instructions.

use serde_json::{Value, json};

use super::{ToolEntry, ToolImpl, ToolOutcome, str_arg};
use crate::provider::ToolSpec;
use crate::skills::Skill;

pub(super) fn entry() -> ToolEntry {
    ToolEntry::new(
        ToolSpec {
            name: "read_skill".into(),
            description: "Load full instructions for an advertised skill.".into(),
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
