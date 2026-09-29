//! Planning-mode parsing, continuation prompts, and state helpers.

use super::{Conversation, ConversationKind, last_undoable_user_index};
use crate::agent::events::TurnEvent;
use crate::error::Error;
use crate::provider::{Message, MessageControl, TokenUsage, ToolCall};
use crate::session::PlanState;
use crate::tools::{
    PLAN_ACTION_TOOL_NAME, PLAN_COMPLETE_TOOL_NAME, PLAN_IMPLEMENTED_TOOL_NAME,
    USER_INPUT_TOOL_NAME,
};

pub(super) const PLAN_CONTINUATION: &str = "Continue planning. Ask another batch of exactly three questions if material choices remain. Otherwise call plan_complete with the full Markdown plan. A normal text reply does not finish planning.";
pub(super) const PLAN_QUESTION_REQUIRED: &str = "Before finishing the plan, call request_user_input with exactly three meaningful multiple-choice questions. Each question needs one recommended option.";
pub(super) const PLAN_IMPLEMENT_CONTINUATION: &str = "Continue implementing the active plan. When it is fully complete, call plan_implemented with the final user-facing result as the only tool call in that step.";

pub(super) const PLAN_MODE_ENDED: &str =
    "Plan mode ended; ignore earlier plan mode updates and follow the conversation normally.";

impl Conversation {
    pub(super) fn plan_file_reference(&self) -> Result<Option<String>, Error> {
        match &self.kind {
            ConversationKind::Persistent(state) => Ok(state
                .session
                .ensure_plan_file()?
                .map(|path| path.to_string_lossy().into_owned())),
            ConversationKind::Child(_) => Ok(None),
        }
    }

    /// Replaces the planning history with the completed plan. The plan is
    /// required to be self-contained, so no summarizer request runs: servers
    /// without a prompt cache would otherwise re-read the whole planning
    /// conversation before implementation starts.
    pub(super) fn prepare_plan_handoff(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let Some(PlanState::Ready { plan, revision }) = self.plan_state() else {
            return Err(Error::Config("no completed plan is active".into()));
        };
        if self.persistent_state().session.plan_has_handoff(*revision) {
            return Ok(());
        }
        let revision = *revision;
        let plan = plan.clone();
        let path = self
            .plan_file_reference()?
            .ok_or_else(|| Error::Config("no saved plan file".into()))?;
        let replaced = last_undoable_user_index(&self.messages)
            .ok_or_else(|| Error::Protocol("plan implementation has no user prompt".into()))?;
        let handoff = format!(
            "Implementation handoff\n\nThe planning conversation was replaced by its completed plan.\n\nSaved plan file: {path}\n\n{plan}"
        );
        self.persistent_mut()
            .session
            .append_plan_handoff(revision, &handoff, replaced)?;
        crate::compaction::apply_summary(&mut self.messages, &handoff, replaced);
        self.context_tokens = 0;
        self.context_usage = None;
        sink(TurnEvent::Usage {
            context_tokens: 0,
            context_window: self.context_window(),
            request_usage: TokenUsage::default(),
            session_usage: self.usage(),
        });
        sink(TurnEvent::Compacted { replaced });
        Ok(())
    }

    /// Appends a hidden phase note when the plan phase differs from the
    /// latest note in history. Notes only ever append, so the prompt prefix
    /// already cached by the server stays valid.
    pub(super) fn sync_plan_phase_note(&mut self, note: Option<String>) -> Result<(), Error> {
        let latest = self
            .messages
            .iter()
            .rev()
            .find(|message| message.control == Some(MessageControl::PlanPhase))
            .map(|message| message.content.as_str());
        let note = match (note, latest) {
            (Some(note), latest) if latest != Some(note.as_str()) => note,
            (None, Some(latest)) if latest != PLAN_MODE_ENDED => PLAN_MODE_ENDED.to_string(),
            _ => return Ok(()),
        };
        self.append_input_message(Message::user(note).with_control(MessageControl::PlanPhase))
    }
}

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
