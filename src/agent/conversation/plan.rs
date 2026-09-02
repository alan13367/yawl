//! Planning-mode parsing, continuation prompts, and state helpers.

use crate::provider::ToolCall;
use crate::tools::{
    PLAN_ACTION_TOOL_NAME, PLAN_COMPLETE_TOOL_NAME, PLAN_IMPLEMENTED_TOOL_NAME,
    USER_INPUT_TOOL_NAME,
};

pub(super) const PLAN_CONTINUATION: &str = "Continue planning. Ask another batch of exactly three questions if material choices remain. Otherwise call plan_complete with the full Markdown plan. A normal text reply does not finish planning.";
pub(super) const PLAN_QUESTION_REQUIRED: &str = "Before finishing the plan, call request_user_input with exactly three meaningful multiple-choice questions. Each question needs one recommended option.";
pub(super) const PLAN_IMPLEMENT_CONTINUATION: &str = "Continue implementing the active plan. When it is fully complete, call plan_implemented with the final user-facing result as the only tool call in that step.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FollowUpAction {
    Revise,
    Implement,
    Unrelated,
}

pub(super) fn parse_plan(arguments: &str) -> Result<String, String> {
    parse_non_empty(arguments, "plan", PLAN_COMPLETE_TOOL_NAME)
}

pub(super) fn parse_implemented(arguments: &str) -> Result<String, String> {
    parse_non_empty(arguments, "result", PLAN_IMPLEMENTED_TOOL_NAME)
}

pub(super) fn parse_action(arguments: &str) -> Result<FollowUpAction, String> {
    let value: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid tool arguments json: {error}"))?;
    match value.get("action").and_then(serde_json::Value::as_str) {
        Some("revise") => Ok(FollowUpAction::Revise),
        Some("implement") => Ok(FollowUpAction::Implement),
        Some("unrelated") => Ok(FollowUpAction::Unrelated),
        _ => Err("plan_action requires action 'revise', 'implement', or 'unrelated'".into()),
    }
}

pub(super) fn is_plan_complete(call: &ToolCall) -> bool {
    call.name == PLAN_COMPLETE_TOOL_NAME
}

pub(super) fn is_plan_action(call: &ToolCall) -> bool {
    call.name == PLAN_ACTION_TOOL_NAME
}

pub(super) fn is_plan_implemented(call: &ToolCall) -> bool {
    call.name == PLAN_IMPLEMENTED_TOOL_NAME
}

pub(super) fn is_user_input(call: &ToolCall) -> bool {
    call.name == USER_INPUT_TOOL_NAME
}

fn parse_non_empty(arguments: &str, key: &str, tool: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid tool arguments json: {error}"))?;
    let Some(result) = value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|result| !result.is_empty())
    else {
        return Err(format!("{tool} requires a non-empty string '{key}'"));
    };
    Ok(result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_completion_requires_non_empty_markdown() {
        assert_eq!(
            parse_plan(r##"{"plan":"# Ship it"}"##).as_deref(),
            Ok("# Ship it")
        );
        assert!(parse_plan(r#"{"plan":"  "}"#).is_err());
    }

    #[test]
    fn follow_up_action_accepts_only_known_actions() {
        assert_eq!(
            parse_action(r#"{"action":"revise"}"#),
            Ok(FollowUpAction::Revise)
        );
        assert_eq!(
            parse_action(r#"{"action":"unrelated"}"#),
            Ok(FollowUpAction::Unrelated)
        );
        assert!(parse_action(r#"{"action":"discuss"}"#).is_err());
    }
}
