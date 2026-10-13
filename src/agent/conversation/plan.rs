//! Planning-mode parsing, continuation prompts, and state helpers.

use super::{Conversation, last_undoable_user_index};
use crate::agent::events::TurnEvent;
use crate::error::Error;
use crate::provider::{Message, MessageControl, Role, TokenUsage, ToolCall};
use crate::session::PlanState;
use crate::tools::{
    PLAN_ACTION_TOOL_NAME, PLAN_COMPLETE_TOOL_NAME, PLAN_IMPLEMENTED_TOOL_NAME,
    USER_INPUT_TOOL_NAME,
};

pub(super) const PLAN_CONTINUATION: &str = "Continue planning. If you have not asked yet, call request_user_input with one to three meaningful questions first; ask further batches only while material choices remain. Otherwise call plan_complete with the full Markdown plan. A normal text reply does not finish planning.";
pub(super) const PLAN_REVISE_CONTINUATION: &str = "Continue revising the plan. Ask one to three questions with request_user_input only if the requested change leaves material choices open. Otherwise call plan_complete with the full revised Markdown plan. A normal text reply does not finish revision.";
pub(super) const PLAN_QUESTION_REQUIRED: &str = "Before finishing the plan, call request_user_input with one to three meaningful multiple-choice questions. Each question needs one recommended option.";
pub(super) const PLAN_IMPLEMENT_CONTINUATION: &str = "Continue implementing the active plan. When it is fully complete, call plan_implemented with the final user-facing result as the only tool call in that step. If only the user can unblock the work, call plan_implemented with status blocked and say what you need.";

pub(super) const PLAN_MODE_ENDED: &str =
    "Plan mode ended; ignore earlier plan mode updates and follow the conversation normally.";

impl Conversation {
    pub(super) fn plan_file_reference(&self) -> Result<Option<String>, Error> {
        Ok(self
            .session
            .ensure_plan_file()?
            .map(|path| path.to_string_lossy().into_owned()))
    }

    /// Replaces the planning conversation, from the prompt that started the
    /// plan up to the implementation prompt, with the completed plan. The
    /// plan is required to be self-contained, so no summarizer request runs:
    /// servers without a prompt cache would otherwise re-read the whole
    /// planning conversation before implementation starts. Conversation from
    /// before the plan stays in context.
    pub(super) fn prepare_plan_handoff(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let Some(PlanState::Ready { plan, revision }) = self.plan_state() else {
            return Err(Error::Invalid("no completed plan is active".into()));
        };
        if self.session.plan_has_handoff(*revision) {
            return Ok(());
        }
        let revision = *revision;
        let plan = plan.clone();
        let path = self
            .plan_file_reference()?
            .ok_or_else(|| Error::Protocol("no saved plan file".into()))?;
        let messages = self.messages();
        let end = last_undoable_user_index(messages)
            .ok_or_else(|| Error::Protocol("plan implementation has no user prompt".into()))?;
        // Sessions from before plan-start markers, or whose start was later
        // compacted away, hand off everything before the prompt.
        let start = messages[..end]
            .iter()
            .rposition(Message::is_plan_start)
            .unwrap_or(0);
        let handoff = handoff_text(&path, &plan, &messages[start..end]);
        self.session
            .append_plan_handoff(revision, &handoff, start..end)?;
        self.context_tokens = 0;
        self.context_usage = None;
        sink(TurnEvent::Usage {
            context_tokens: 0,
            context_window: self.context_window(),
            request_usage: TokenUsage::default(),
            session_usage: self.usage(),
        });
        sink(TurnEvent::PlanHandoff {
            path: &path,
            replaced: end - start,
        });
        Ok(())
    }

    /// Appends a hidden phase note when the plan phase differs from the
    /// latest note in history. Notes only ever append, so the prompt prefix
    /// already cached by the server stays valid.
    pub(super) fn sync_plan_phase_note(&mut self, note: Option<String>) -> Result<(), Error> {
        let latest = self
            .messages()
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

/// Longest user prompt carried verbatim into a handoff. Answers to planning
/// questions are never shortened because they are requirements.
const HANDOFF_PROMPT_CHARS: usize = 2_000;

/// Builds the message that replaces `planning`: the objective, the user's
/// answers and prompts in order, then the plan. Implementation starts from
/// this alone, so requirements the plan omits are not lost with the history.
fn handoff_text(path: &str, plan: &str, planning: &[Message]) -> String {
    let (objective, rest) = match planning.split_first() {
        Some((first, rest)) if first.is_plan_start() => (Some(first.content.trim()), rest),
        _ => (None, planning),
    };
    let record = planning_record(rest);
    let mut text = String::from(
        "Implementation handoff: the planning conversation was replaced by its completed plan; the original messages remain in the session log. The objective and the user's answers below are the requirements the plan was built from, oldest first, so later entries supersede earlier ones. Follow them where the plan is silent.\n\n",
    );
    text.push_str("Saved plan file: `");
    text.push_str(path);
    text.push('`');
    if let Some(objective) = objective.filter(|objective| !objective.is_empty()) {
        text.push_str("\n\nObjective:\n");
        for line in objective.lines() {
            text.push_str("> ");
            text.push_str(line);
            text.push('\n');
        }
        text.pop();
    }
    if !record.is_empty() {
        text.push_str("\n\nDuring planning:\n");
        text.push_str(&record.join("\n"));
    }
    text.push_str("\n\nPlan:\n\n");
    text.push_str(plan.trim());
    text
}

/// The user's side of the planning conversation as Markdown list items:
/// question answers paired with their questions, and visible prompts.
fn planning_record(messages: &[Message]) -> Vec<String> {
    let mut record = Vec::new();
    for message in messages {
        match message.role {
            Role::User
                if !message.is_hidden_control()
                    && message.subagent_results.is_empty()
                    && !message.is_summary()
                    && !message.is_plan_handoff() =>
            {
                let prompt = crate::error::truncate(message.content.trim(), HANDOFF_PROMPT_CHARS);
                if !prompt.is_empty() {
                    record.push(format!("- User: {}", prompt.replace('\n', "\n  ")));
                }
            }
            Role::Assistant => {
                for call in message.tool_calls.iter().filter(|call| is_user_input(call)) {
                    let answer = messages.iter().find(|result| {
                        result.role == Role::Tool
                            && !result.is_error
                            && result.tool_call_id.as_deref() == Some(call.id.as_str())
                    });
                    if let Some(answer) = answer {
                        record.extend(answered_questions(&call.arguments, &answer.content));
                    }
                }
            }
            Role::User | Role::Tool => {}
        }
    }
    record
}

/// Pairs each answer from a `request_user_input` result with its question.
fn answered_questions(arguments: &str, result: &str) -> Vec<String> {
    let (Ok(arguments), Ok(result)) = (
        serde_json::from_str::<serde_json::Value>(arguments),
        serde_json::from_str::<serde_json::Value>(result),
    ) else {
        return Vec::new();
    };
    let questions = arguments
        .get("questions")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let answers = result
        .get("answers")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let text = |value: &serde_json::Value, key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    answers
        .iter()
        .filter_map(|answer| {
            let id = text(answer, "id");
            let response = text(answer, "answer");
            if response.is_empty() {
                return None;
            }
            let question = questions
                .iter()
                .find(|question| text(question, "id") == id)
                .map(|question| text(question, "question"))
                .filter(|question| !question.is_empty())
                .unwrap_or(id);
            let defaulted = if text(answer, "source") == "timeout" {
                " (recommended default; the user did not answer)"
            } else {
                ""
            };
            Some(format!(
                "- Q: {}\n  A: {}{defaulted}",
                question.replace('\n', " "),
                response.replace('\n', "\n  ")
            ))
        })
        .collect()
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

/// How a `plan_implemented` call ends the implementation turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImplementationStatus {
    Completed,
    /// Only the user can unblock the work; the plan stays active.
    Blocked,
}

pub(super) fn parse_implemented(arguments: &str) -> Result<(String, ImplementationStatus), String> {
    let value = parse_arguments(arguments)?;
    let result = non_empty_string(&value, "result", PLAN_IMPLEMENTED_TOOL_NAME)?;
    let status = match value.get("status").and_then(serde_json::Value::as_str) {
        None | Some("completed") => ImplementationStatus::Completed,
        Some("blocked") => ImplementationStatus::Blocked,
        Some(_) => {
            return Err("plan_implemented status must be 'completed' or 'blocked'".into());
        }
    };
    Ok((result, status))
}

pub(super) fn parse_action(arguments: &str) -> Result<FollowUpAction, String> {
    let value = parse_arguments(arguments)?;
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

/// The trimmed, non-empty string `key` of a finishing tool's JSON arguments.
pub(super) fn parse_non_empty(arguments: &str, key: &str, tool: &str) -> Result<String, String> {
    non_empty_string(&parse_arguments(arguments)?, key, tool)
}

fn parse_arguments(arguments: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(arguments).map_err(|error| format!("invalid tool arguments json: {error}"))
}

fn non_empty_string(value: &serde_json::Value, key: &str, tool: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|result| !result.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{tool} requires a non-empty string '{key}'"))
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
    fn implementation_can_finish_completed_or_blocked() {
        assert_eq!(
            parse_implemented(r#"{"result":"Shipped"}"#),
            Ok(("Shipped".into(), ImplementationStatus::Completed))
        );
        assert_eq!(
            parse_implemented(r#"{"result":"Need a key","status":"blocked"}"#),
            Ok(("Need a key".into(), ImplementationStatus::Blocked))
        );
        assert!(parse_implemented(r#"{"result":"x","status":"partial"}"#).is_err());
        assert!(parse_implemented(r#"{"status":"blocked"}"#).is_err());
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
