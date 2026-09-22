//! Planning-mode parsing, continuation prompts, and state helpers.

use super::{Conversation, ConversationKind, last_undoable_user_index};
use crate::agent::events::TurnEvent;
use crate::config::Config;
use crate::error::Error;
use crate::provider::ToolCall;
use crate::provider::{self, Message, Request, stream_turn};
use crate::session::PlanState;
use crate::tools::{
    PLAN_ACTION_TOOL_NAME, PLAN_COMPLETE_TOOL_NAME, PLAN_IMPLEMENTED_TOOL_NAME,
    USER_INPUT_TOOL_NAME,
};

pub(super) const PLAN_CONTINUATION: &str = "Continue planning. Ask another batch of exactly three questions if material choices remain. Otherwise call plan_complete with the full Markdown plan. A normal text reply does not finish planning.";
pub(super) const PLAN_QUESTION_REQUIRED: &str = "Before finishing the plan, call request_user_input with exactly three meaningful multiple-choice questions. Each question needs one recommended option.";
pub(super) const PLAN_IMPLEMENT_CONTINUATION: &str = "Continue implementing the active plan. When it is fully complete, call plan_implemented with the final user-facing result as the only tool call in that step.";

const HANDOFF_SYSTEM: &str = "Summarize the earlier conversation for implementation of its completed plan. Preserve user constraints, decisions and reasons, relevant code locations and investigation findings, unresolved issues, and work already performed. The full active plan will be supplied separately: do not repeat it. Target roughly 1,000 tokens of dense factual text. Preserve essential details rather than truncating them. Return only the summary, without tool calls.";

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

    pub(super) fn prepare_plan_handoff<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
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
        sink(TurnEvent::Compacting);
        let ask = Message::user(format!(
            "Summarize this conversation. The active plan is included in its history and will also be supplied to the implementer separately.\n\n{}",
            crate::compaction::transcript(&self.messages[..replaced])
        ));
        let (provider, model) = resolve_provider(&self.model, &self.config)?;
        let request = Request {
            model: &model,
            system: HANDOFF_SYSTEM,
            messages: std::slice::from_ref(&ask),
            tools: &[],
            max_tokens: crate::model::max_tokens(&self.config, &self.model),
            supports_images: false,
            prompt_cache_control: false,
            prompt_cache_key: None,
        };
        let out = stream_turn(provider.as_ref(), &request, &mut |_| {})?;
        self.record_usage(out.usage)?;
        if crate::cancellation::interrupted() {
            return Err(Error::Interrupted);
        }
        if out.text.trim().is_empty() || !out.tool_calls.is_empty() {
            return Err(Error::Protocol(
                "plan handoff summarizer must return non-empty text without tool calls".into(),
            ));
        }
        let handoff = format!(
            "Implementation handoff\n\n{}\n\nSaved plan file: {path}\n\n{plan}",
            out.text.trim()
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
            request_usage: out.usage,
            session_usage: self.usage(),
        });
        sink(TurnEvent::Compacted { replaced });
        Ok(())
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
