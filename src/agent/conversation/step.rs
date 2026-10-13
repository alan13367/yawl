//! One model step: what a response asks for and how the turn answers it.
//!
//! [`classify`] decides from the response alone, in a fixed order, and each
//! kind has one handler. Handlers record the response, run or reject its
//! tool calls, and tell the turn loop whether to request another step.

use crate::error::Error;
use crate::provider::{Message, MessageControl, ToolCall};

use super::Conversation;
use super::goal;
use super::plan::{self, FollowUpAction, ImplementationStatus};
use super::turn::{Step, TurnMode, TurnProgress, plan_continuation};
use crate::agent::events::TurnEvent;

/// Consecutive text-only replies a plan phase accepts before stopping the
/// turn, so a model that is blocked or ignores the finishing tool cannot
/// loop on continuation prompts forever.
pub(super) const MAX_IDLE_PLAN_REPLIES: u32 = 3;

/// What a model response asks for, in the order the turn handles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StepKind {
    /// The user steered while the model answered: record the response,
    /// skip its tool calls, and take the steer.
    Steered,
    /// `request_user_input` asks the user questions.
    Questions,
    /// A private planning tool finishes or routes a plan phase.
    PlanTool,
    /// `goal_complete` in goal mode.
    GoalComplete,
    /// Plain text without tool calls.
    Reply,
    /// Ordinary tool calls.
    Tools,
}

pub(super) fn classify(mode: TurnMode, calls: &[ToolCall], steered: bool) -> StepKind {
    if steered {
        StepKind::Steered
    } else if calls.iter().any(plan::is_user_input) {
        StepKind::Questions
    } else if calls.iter().any(|call| {
        plan::is_plan_complete(call)
            || plan::is_plan_action(call)
            || plan::is_plan_implemented(call)
    }) {
        StepKind::PlanTool
    } else if mode == TurnMode::Goal && calls.iter().any(goal::is_goal_complete) {
        StepKind::GoalComplete
    } else if calls.is_empty() {
        StepKind::Reply
    } else {
        StepKind::Tools
    }
}

/// Whether the turn requests another model step.
pub(super) enum Flow {
    Continue,
    /// The turn ends: `true` when it completed, `false` when interrupted.
    Finish(bool),
}

impl Conversation {
    pub(super) fn handle_step(
        &mut self,
        kind: StepKind,
        assistant: Message,
        calls: &[ToolCall],
        step: &Step,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        match kind {
            StepKind::Steered => self.on_steered(assistant, calls, progress.mode, sink),
            StepKind::Questions => self.on_questions(assistant, calls, step, progress, sink),
            StepKind::PlanTool => self.on_plan_tool(assistant, calls, step, progress, sink),
            StepKind::GoalComplete => self.on_goal_complete(assistant, calls, step, sink),
            StepKind::Reply => self.on_reply(assistant, progress, sink),
            StepKind::Tools => self.on_tools(assistant, calls, step, sink),
        }
    }

    fn on_steered(
        &mut self,
        assistant: Message,
        calls: &[ToolCall],
        mode: TurnMode,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        self.persist_assistant(assistant, sink)?;
        self.skip_tool_calls(calls, goal::STEER_SKIPPED)?;
        if self.inject_pending_steers(sink)? || !calls.is_empty() {
            return Ok(Flow::Continue);
        }
        if mode == TurnMode::Goal {
            self.append_goal_continuation()?;
            return Ok(Flow::Continue);
        }
        if let Some(continuation) = plan_continuation(mode) {
            self.append_plan_continuation(continuation)?;
            return Ok(Flow::Continue);
        }
        Ok(Flow::Finish(true))
    }

    fn on_questions(
        &mut self,
        assistant: Message,
        calls: &[ToolCall],
        step: &Step,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        self.persist_assistant(assistant, sink)?;
        let [call] = calls else {
            self.reject_tool_calls(
                calls,
                "request_user_input must be the only tool call in its model step",
            )?;
            return Ok(Flow::Continue);
        };
        if let Err(error) = crate::tools::validate_user_input(&call.arguments)
            && matches!(progress.mode, TurnMode::Plan | TurnMode::PlanRevise)
        {
            self.reject_tool_calls(calls, &error)?;
            return Ok(Flow::Continue);
        }
        if self.run_tools(&step.registry, calls, sink)? {
            return Ok(Flow::Finish(false));
        }
        // Only drafting requires questions; revisions ask optionally.
        if progress.mode == TurnMode::Plan {
            progress.asked_plan_questions = true;
            self.session.append_plan_questions_asked()?;
        }
        Ok(Flow::Continue)
    }

    fn on_plan_tool(
        &mut self,
        assistant: Message,
        calls: &[ToolCall],
        step: &Step,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        let [call] = calls else {
            self.persist_assistant(assistant, sink)?;
            self.reject_tool_calls(
                calls,
                "private planning tools must be the only tool call in their model step",
            )?;
            return Ok(Flow::Continue);
        };
        let mode = progress.mode;
        let rejection = match mode {
            TurnMode::Plan | TurnMode::PlanRevise if plan::is_plan_complete(call) => {
                if mode == TurnMode::Plan && !progress.asked_plan_questions {
                    Some(plan::PLAN_QUESTION_REQUIRED.to_string())
                } else {
                    match plan::parse_plan(&call.arguments) {
                        Ok(plan) => {
                            self.finish_plan_complete(assistant, plan, sink)?;
                            return Ok(Flow::Finish(true));
                        }
                        Err(error) => Some(error),
                    }
                }
            }
            TurnMode::PlanFollowUp if plan::is_plan_action(call) => {
                match plan::parse_action(&call.arguments) {
                    Ok(action) => {
                        self.persist_assistant(assistant, sink)?;
                        if self.run_tools(&step.registry, calls, sink)? {
                            return Ok(Flow::Finish(false));
                        }
                        progress.mode = match action {
                            FollowUpAction::Revise => TurnMode::PlanRevise,
                            FollowUpAction::Implement => TurnMode::PlanImplement,
                            FollowUpAction::Unrelated => TurnMode::Normal,
                        };
                        return Ok(Flow::Continue);
                    }
                    Err(error) => Some(error),
                }
            }
            TurnMode::PlanImplement if plan::is_plan_implemented(call) => {
                match plan::parse_implemented(&call.arguments) {
                    Ok((result, ImplementationStatus::Completed)) => {
                        self.finish_plan_implemented(assistant, result, sink)?;
                        return Ok(Flow::Finish(true));
                    }
                    Ok((result, ImplementationStatus::Blocked)) => {
                        self.finish_plan_blocked(assistant, result, sink)?;
                        return Ok(Flow::Finish(true));
                    }
                    Err(error) => Some(error),
                }
            }
            _ => Some("that planning tool is unavailable in the current phase".into()),
        };
        if let Some(error) = rejection {
            self.persist_assistant(assistant, sink)?;
            self.reject_tool_calls(calls, &error)?;
        }
        Ok(Flow::Continue)
    }

    fn on_goal_complete(
        &mut self,
        assistant: Message,
        calls: &[ToolCall],
        step: &Step,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        if let [call] = calls {
            match goal::parse_goal_complete_result(&call.arguments) {
                Ok(result) => {
                    self.finish_goal_complete(assistant, result, sink)?;
                    return Ok(Flow::Finish(true));
                }
                Err(error) => {
                    self.persist_assistant(assistant, sink)?;
                    self.reject_tool_calls(calls, &error)?;
                    return Ok(Flow::Continue);
                }
            }
        }
        self.persist_assistant(assistant, sink)?;
        if self.run_tools_rejecting_goal_complete(
            &step.registry,
            calls,
            sink,
            "goal_complete must be the only tool call in its step",
        )? {
            return Ok(Flow::Finish(false));
        }
        self.inject_pending_steers(sink)?;
        Ok(Flow::Continue)
    }

    fn on_reply(
        &mut self,
        assistant: Message,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        self.persist_assistant(assistant, sink)?;
        if self.inject_pending_steers(sink)? {
            return Ok(Flow::Continue);
        }
        let mode = progress.mode;
        if mode == TurnMode::Goal {
            self.append_goal_continuation()?;
            return Ok(Flow::Continue);
        }
        if let Some(continuation) = plan_continuation(mode) {
            progress.idle_plan_replies += 1;
            if progress.idle_plan_replies >= MAX_IDLE_PLAN_REPLIES {
                sink(TurnEvent::Warning(idle_plan_stop_notice(mode)));
                return Ok(Flow::Finish(true));
            }
            self.append_plan_continuation(continuation)?;
            return Ok(Flow::Continue);
        }
        Ok(Flow::Finish(true))
    }

    fn on_tools(
        &mut self,
        assistant: Message,
        calls: &[ToolCall],
        step: &Step,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Flow, Error> {
        self.persist_assistant(assistant, sink)?;
        if self.run_tools(&step.registry, calls, sink)? {
            return Ok(Flow::Finish(false));
        }
        self.inject_pending_steers(sink)?;
        Ok(Flow::Continue)
    }

    fn persist_assistant(
        &mut self,
        assistant: Message,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        self.session.append_message(assistant)?;
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn skip_tool_calls(&mut self, calls: &[ToolCall], reason: &str) -> Result<(), Error> {
        for call in calls {
            let result = Message::tool_result(&call.id, &call.name, reason.to_string(), true)
                .with_control(MessageControl::ToolSkipped);
            self.session.append_message(result)?;
        }
        Ok(())
    }

    fn reject_tool_calls(&mut self, calls: &[ToolCall], error: &str) -> Result<(), Error> {
        for call in calls {
            let result = Message::tool_result(&call.id, &call.name, error.to_string(), true);
            self.session.append_message(result)?;
        }
        Ok(())
    }

    fn append_goal_continuation(&mut self) -> Result<(), Error> {
        self.append_input_message(
            Message::user(goal::GOAL_CONTINUATION).with_control(MessageControl::GoalContinuation),
        )
    }

    fn append_plan_continuation(&mut self, text: &str) -> Result<(), Error> {
        self.append_input_message(
            Message::user(text).with_control(MessageControl::PlanContinuation),
        )
    }

    fn finish_goal_complete(
        &mut self,
        mut assistant: Message,
        result: String,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        show_final_text(&assistant.content, &result, sink);
        settle_final_text(&mut assistant, &result);
        self.session.append_goal_complete(assistant)?;
        self.latest_turn_result = result;
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn finish_plan_complete(
        &mut self,
        mut assistant: Message,
        plan: String,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let streamed = std::mem::take(&mut assistant.content);
        settle_final_text(&mut assistant, &plan);
        let path = self.session.append_plan_ready(&plan, assistant)?;
        show_final_text(&streamed, &plan, sink);
        if let Some(path) = path {
            sink(TurnEvent::TextDelta(&format!(
                "\n\nSaved plan: {}",
                path.display()
            )));
        }
        self.latest_turn_result = plan;
        self.plan_ready_this_turn = true;
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn finish_plan_implemented(
        &mut self,
        mut assistant: Message,
        result: String,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        show_final_text(&assistant.content, &result, sink);
        settle_final_text(&mut assistant, &result);
        self.session.append_plan_implemented(assistant)?;
        self.latest_turn_result = result;
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    /// Ends an implementation turn the model cannot finish without the
    /// user. The plan stays active, so a reply continues implementing.
    fn finish_plan_blocked(
        &mut self,
        mut assistant: Message,
        result: String,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        show_final_text(&assistant.content, &result, sink);
        settle_final_text(&mut assistant, &result);
        self.session.append_message(assistant)?;
        self.latest_turn_result = result;
        sink(TurnEvent::AssistantDone);
        sink(TurnEvent::Warning(
            "Implementation is blocked and needs your input. The plan stays active: reply or run /plan implement to continue, or /plan cancel."
                .into(),
        ));
        Ok(())
    }
}

/// Shows a finishing tool's `result` in place of the text the model
/// streamed: streams it when nothing streamed, replaces what streamed when
/// it differs.
fn show_final_text(streamed: &str, result: &str, sink: &mut dyn FnMut(TurnEvent<'_>)) {
    if streamed.is_empty() {
        sink(TurnEvent::TextDelta(result));
    } else if streamed != result {
        sink(TurnEvent::AssistantReplace(result));
    }
}

/// Records the finishing tool's result as the response text. The finishing
/// call itself is not kept, so the history has no unanswered tool call.
fn settle_final_text(assistant: &mut Message, result: &str) {
    result.clone_into(&mut assistant.content);
    assistant.tool_calls.clear();
}

fn idle_plan_stop_notice(mode: TurnMode) -> String {
    let next = match mode {
        TurnMode::Plan => "/plan resume continues planning; /plan cancel clears it.",
        TurnMode::PlanRevise => "The plan is unchanged. Reply to keep revising, or /plan cancel.",
        _ => "The plan stays active. Reply or run /plan implement to continue, or /plan cancel.",
    };
    format!(
        "Stopped after {MAX_IDLE_PLAN_REPLIES} replies in a row that did not finish this plan step. {next}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn steps_are_classified_in_handling_order() {
        let questions = call(crate::tools::USER_INPUT_TOOL_NAME);
        let plan_complete = call(crate::tools::PLAN_COMPLETE_TOOL_NAME);
        let goal_complete = call(crate::tools::GOAL_COMPLETE_TOOL_NAME);
        let shell = call("shell");
        // A pending steer wins over everything the response asked for.
        assert_eq!(
            classify(TurnMode::Plan, std::slice::from_ref(&questions), true),
            StepKind::Steered
        );
        assert_eq!(
            classify(TurnMode::Plan, &[shell.clone(), questions], false),
            StepKind::Questions
        );
        assert_eq!(
            classify(TurnMode::Normal, &[plan_complete], false),
            StepKind::PlanTool
        );
        assert_eq!(
            classify(
                TurnMode::Goal,
                &[shell.clone(), goal_complete.clone()],
                false
            ),
            StepKind::GoalComplete
        );
        // Outside goal mode, goal_complete is an ordinary (unknown) tool.
        assert_eq!(
            classify(TurnMode::Normal, &[goal_complete], false),
            StepKind::Tools
        );
        assert_eq!(classify(TurnMode::Normal, &[], false), StepKind::Reply);
        assert_eq!(classify(TurnMode::Normal, &[shell], false), StepKind::Tools);
    }
}
