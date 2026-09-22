//! Single-call entries that move a conversation between modes: finishing a
//! goal or planning turn, and classifying the next request against a plan.

use serde_json::{Value, json};

use super::{
    GOAL_COMPLETE_TOOL_NAME, PLAN_ACTION_TOOL_NAME, PLAN_COMPLETE_TOOL_NAME,
    PLAN_IMPLEMENTED_TOOL_NAME, ToolEntry, ToolImpl, ToolOutcome,
};
use crate::provider::ToolSpec;

pub(super) fn goal_complete_entry() -> ToolEntry {
    ToolEntry::new(ToolSpec {
            name: GOAL_COMPLETE_TOOL_NAME.into(),
            description: "Finish the active goal with the final user-facing answer. This must be the only tool call in that step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "result": {
                        "type": "string",
                        "description": "Final user-facing answer for the completed goal"
                    }
                },
                "required": ["result"]
            }),
        }, ToolImpl::GoalComplete,
    )
}

pub(super) fn plan_complete_entry() -> ToolEntry {
    ToolEntry::new(ToolSpec {
            name: PLAN_COMPLETE_TOOL_NAME.into(),
            description: "Finish planning with the complete Markdown plan. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"plan": {"type": "string"}},
                "required": ["plan"]
            }),
        }, ToolImpl::PlanComplete,
    )
}

pub(super) fn plan_action_entry() -> ToolEntry {
    ToolEntry::new(ToolSpec {
            name: PLAN_ACTION_TOOL_NAME.into(),
            description: "Classify the user's latest request against the active plan. Use unrelated to continue the request as a normal turn with the full tool set. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"action": {"type": "string", "enum": ["revise", "implement", "unrelated"]}},
                "required": ["action"]
            }),
        }, ToolImpl::PlanAction,
    )
}

pub(super) fn plan_implemented_entry() -> ToolEntry {
    ToolEntry::new(ToolSpec {
            name: PLAN_IMPLEMENTED_TOOL_NAME.into(),
            description: "Finish implementation of the active plan with the final user-facing result. This must be the only tool call in its step.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"result": {"type": "string"}},
                "required": ["result"]
            }),
        }, ToolImpl::PlanImplemented,
    )
}

pub(super) fn plan_action_outcome(args: &Value) -> ToolOutcome {
    match args.get("action").and_then(Value::as_str) {
        Some(action @ ("revise" | "implement" | "unrelated")) => ToolOutcome::ok(action.into()),
        _ => {
            ToolOutcome::error("plan_action requires action 'revise', 'implement', or 'unrelated'")
        }
    }
}

pub(super) fn non_empty_arg(args: &Value, key: &str, tool: &str) -> ToolOutcome {
    match args.get(key).and_then(Value::as_str).map(str::trim) {
        Some(value) if !value.is_empty() => ToolOutcome::ok(value.to_string()),
        _ => ToolOutcome::error(format!("{tool} requires a non-empty string '{key}'")),
    }
}
