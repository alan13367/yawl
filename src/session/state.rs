//! The session reducer. Every event changes the history and the derived
//! session state here and nowhere else, so a live session and a replay of
//! its log always agree.

use std::collections::BTreeSet;

use super::{ContextUsage, PendingUndo, PlanState, SessionEvent};
use crate::provider::{Message, UsageSummary};

#[derive(Debug, Default)]
pub(super) struct SessionState {
    pub(super) messages: Vec<Message>,
    /// Last model used in this log: the header model unless a later switch
    /// event exists.
    pub(super) model: String,
    /// `None` when the session has no reasoning choice of its own.
    /// `Some(None)` is an explicit provider default. `Some(Some(level))` is
    /// an explicit effort.
    pub(super) reasoning_effort: Option<Option<String>>,
    pub(super) active_goal: Option<String>,
    pub(super) active_plan: Option<PlanState>,
    /// Plan state when the latest user turn began, which `/undo` restores.
    pub(super) plan_before_turn: Option<PlanState>,
    /// Text of every completed plan revision, oldest first. Revision `n` is
    /// `plan_revisions[n - 1]`.
    pub(super) plan_revisions: Vec<String>,
    /// Plan revisions whose planning conversation was already handed off.
    pub(super) plan_handoffs: BTreeSet<usize>,
    pub(super) usage: UsageSummary,
    pub(super) pending_undo: Option<Box<PendingUndo>>,
    pub(super) context: Option<ContextUsage>,
}

impl SessionState {
    pub(super) fn apply(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Meta { model, .. } | SessionEvent::ModelSwitch { model } => {
                self.model = model;
            }
            SessionEvent::ReasoningEffort { effort, inherit } => {
                self.reasoning_effort = if inherit { None } else { Some(effort) };
            }
            SessionEvent::Message { message } => self.push(message),
            SessionEvent::Compaction {
                plan_revision,
                summary,
                start,
                replaced,
                provider_data,
                provider_data_model,
            } => {
                let replacement = match plan_revision {
                    Some(revision) => {
                        self.plan_handoffs.insert(revision);
                        Message::plan_handoff(&summary)
                    }
                    None => Message::summary(&summary, provider_data, provider_data_model),
                };
                let start = start.min(self.messages.len());
                let end = start.saturating_add(replaced).min(self.messages.len());
                drop(
                    self.messages
                        .splice(start..end, std::iter::once(replacement)),
                );
                self.context = None;
                self.usage.record_cache_reset();
            }
            SessionEvent::Undo {
                dropped,
                clear_goal,
                plan_state,
            } => {
                let keep = self.messages.len().saturating_sub(dropped);
                self.messages.truncate(keep);
                if clear_goal {
                    self.active_goal = None;
                }
                self.active_plan = self.restore_revision(plan_state);
                self.plan_before_turn = None;
                self.context = None;
                if let Some(undo) = &mut self.pending_undo {
                    undo.committed = true;
                }
            }
            SessionEvent::UndoStarted { mut undo } => {
                undo.plan_state = self.restore_revision(undo.plan_state);
                self.pending_undo = Some(Box::new(undo));
            }
            SessionEvent::UndoFinished => self.pending_undo = None,
            SessionEvent::ToolResultsRecovered { index, results } => {
                let index = index.min(self.messages.len());
                drop(self.messages.splice(index..index, results));
                self.context = None;
            }
            SessionEvent::Context { context } => self.context = Some(context),
            SessionEvent::GoalStart { goal, message } => {
                self.push(message);
                self.active_goal = Some(goal);
            }
            SessionEvent::GoalComplete { message } => {
                self.push(message);
                self.active_goal = None;
            }
            SessionEvent::GoalCancel => self.active_goal = None,
            SessionEvent::PlanStart {
                objective,
                message,
                previous,
            } => {
                // The event names the plan it replaced, so it wins over the
                // generic turn-start snapshot.
                self.messages.push(message);
                self.plan_before_turn = self.restore_revision(previous);
                self.active_plan = Some(PlanState::Draft {
                    objective,
                    questions_asked: false,
                });
            }
            SessionEvent::PlanReady { plan, message } => {
                self.push(message);
                self.plan_revisions.push(plan.clone());
                self.active_plan = Some(PlanState::Ready {
                    plan,
                    revision: self.plan_revisions.len(),
                });
            }
            SessionEvent::PlanQuestionsAsked => {
                if let Some(PlanState::Draft {
                    questions_asked, ..
                }) = &mut self.active_plan
                {
                    *questions_asked = true;
                }
            }
            SessionEvent::PlanImplemented { message } => {
                self.push(message);
                self.active_plan = None;
            }
            SessionEvent::PlanCancel => {
                self.active_plan = None;
                self.plan_before_turn = None;
            }
            SessionEvent::Usage { usage } => self.usage.record(usage),
        }
    }

    /// Appends one message. A message that starts a user turn records the
    /// plan state `/undo` restores when it drops that turn.
    fn push(&mut self, message: Message) {
        if message.starts_user_turn() {
            self.plan_before_turn.clone_from(&self.active_plan);
        }
        self.messages.push(message);
    }

    /// Undo records written before revision IDs were introduced still contain
    /// the plan text, which identifies the latest matching earlier revision.
    fn restore_revision(&self, mut state: Option<PlanState>) -> Option<PlanState> {
        if let Some(PlanState::Ready { plan, revision }) = &mut state
            && *revision == 0
        {
            *revision = self
                .plan_revisions
                .iter()
                .rposition(|saved| saved == plan)
                .map_or(0, |index| index + 1);
        }
        state
    }
}
