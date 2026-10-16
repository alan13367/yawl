//! Goal-mode helpers: completion parsing and continuation text.

use crate::provider::ToolCall;
use crate::tools::GOAL_COMPLETE_TOOL_NAME;

pub(super) const GOAL_CONTINUATION: &str = "Continue working on the active goal. Do not stop until you call goal_complete with the final user-facing result.";

pub(super) fn parse_goal_complete_result(arguments: &str) -> Result<String, String> {
    super::plan::parse_non_empty(arguments, "result", GOAL_COMPLETE_TOOL_NAME)
}

pub(super) fn is_goal_complete(call: &ToolCall) -> bool {
    call.name == GOAL_COMPLETE_TOOL_NAME
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
