//! Goal-mode helpers: completion parsing, continuation text, and skip labels.

use crate::provider::ToolCall;
use crate::tools::GOAL_COMPLETE_TOOL_NAME;

pub(super) const GOAL_CONTINUATION: &str = "Continue working on the active goal. Do not stop until you call goal_complete with the final user-facing result.";

pub(super) const STEER_SKIPPED: &str = "[skipped because the user steered]";

pub(super) fn parse_goal_complete_result(arguments: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(arguments)
        .map_err(|error| format!("invalid tool arguments json: {error}"))?;
    let Some(result) = value.get("result").and_then(serde_json::Value::as_str) else {
        return Err("goal_complete requires a non-empty string 'result'".into());
    };
    let result = result.trim();
    if result.is_empty() {
        return Err("goal_complete requires a non-empty string 'result'".into());
    }
    Ok(result.to_string())
}

pub(super) fn is_goal_complete(call: &ToolCall) -> bool {
    call.name == GOAL_COMPLETE_TOOL_NAME
}

pub(super) fn split_goal_complete(calls: &[ToolCall]) -> (Vec<&ToolCall>, Vec<&ToolCall>) {
    let mut completes = Vec::new();
    let mut ordinary = Vec::new();
    for call in calls {
        if is_goal_complete(call) {
            completes.push(call);
        } else {
            ordinary.push(call);
        }
    }
    (completes, ordinary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_goal_complete_requires_a_non_empty_result() {
        assert_eq!(
            parse_goal_complete_result(r#"{"result":"shipped"}"#).as_deref(),
            Ok("shipped")
        );
        assert!(parse_goal_complete_result(r#"{"result":"  "}"#).is_err());
        assert!(parse_goal_complete_result(r#"{"result":""}"#).is_err());
        assert!(parse_goal_complete_result(r#"{"other":"x"}"#).is_err());
        assert!(parse_goal_complete_result("not-json").is_err());
    }
}
