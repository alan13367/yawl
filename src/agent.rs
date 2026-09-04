//! The agent loop: send messages, stream, execute tool calls, append results,
//! and repeat until the model stops calling tools. Iterations are uncapped;
//! Ctrl+C aborts the in-flight turn, not the process.

mod conversation;
mod events;

use crate::background::BackgroundProcessManager;
use crate::cancellation::CancellationToken;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{Message, TurnInput, UsageSummary};
use crate::session::{PlanState, Session};
use crate::subagent::SubagentManager;
use crate::tools::Registry;

pub use conversation::UndoReport;
pub(crate) use conversation::{Conversation, RunLimits, SteerInbox};
pub use events::TurnEvent;

/// A persistent user-facing conversation.
pub struct Agent {
    conversation: Conversation,
}

impl Agent {
    pub fn new(config: Config, model: String, session: Session, messages: Vec<Message>) -> Self {
        Self {
            conversation: Conversation::persistent(
                config,
                model,
                session,
                messages,
                crate::config::working_dir(),
            ),
        }
    }

    pub fn context_window(&self) -> u64 {
        self.conversation.context_window()
    }

    pub fn config(&self) -> &Config {
        self.conversation.config()
    }

    pub fn model(&self) -> &str {
        self.conversation.model()
    }

    pub fn session_id(&self) -> &str {
        self.conversation.session_id()
    }

    pub fn messages(&self) -> &[Message] {
        self.conversation.messages()
    }

    pub fn context_tokens(&self) -> u64 {
        self.conversation.context_tokens()
    }

    pub fn usage(&self) -> UsageSummary {
        self.conversation.usage()
    }

    pub(crate) fn subagents(&self) -> SubagentManager {
        self.conversation.subagent_manager()
    }

    pub(crate) fn background_processes(&self) -> BackgroundProcessManager {
        self.conversation.background_manager()
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.conversation.cancellation_token()
    }

    pub(crate) fn steer_inbox(&self) -> SteerInbox {
        self.conversation.steer_inbox()
    }

    pub(crate) fn question_broker(&self) -> crate::tools::QuestionBroker {
        self.conversation.question_broker()
    }

    pub(crate) fn enable_interactive_questions(&self) {
        self.conversation.enable_interactive_questions();
    }

    pub(crate) fn active_goal(&self) -> Option<&str> {
        self.conversation.active_goal()
    }

    pub(crate) fn plan_state(&self) -> Option<&PlanState> {
        self.conversation.plan_state()
    }

    pub(crate) fn active_plan(&self) -> Option<&str> {
        self.conversation.active_plan()
    }

    pub(crate) fn plan_ready_this_turn(&self) -> bool {
        self.conversation.plan_ready_this_turn()
    }

    pub(crate) fn take_unaccepted_steers(&self) -> Vec<TurnInput> {
        self.conversation.take_unaccepted_steers()
    }

    pub(crate) fn start_goal(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        self.conversation.start_goal(input)
    }

    pub(crate) fn cancel_goal(&mut self) -> Result<bool, Error> {
        self.conversation.cancel_goal()
    }

    pub(crate) fn start_plan(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        self.conversation.start_plan(input)
    }

    pub(crate) fn cancel_plan(&mut self) -> Result<bool, Error> {
        self.conversation.cancel_plan()
    }

    pub(crate) fn clear_cancellation(&self) {
        self.conversation.clear_cancellation();
    }

    /// Disables automatic subagent follow-ups and adjusts orchestration
    /// guidance for a one-shot print-mode run.
    pub fn set_print_mode(&mut self) {
        self.conversation.set_print_mode();
    }

    pub(crate) fn switch_model(&mut self, model: String) {
        self.conversation.switch_model(model);
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.conversation.set_reasoning_effort(effort);
    }

    pub(crate) fn sync_display_config(&mut self, config: &Config) {
        self.conversation.sync_display_config(config);
    }

    pub fn reset(&mut self) -> Result<(), Error> {
        self.conversation.reset()
    }

    pub fn delete_session(&mut self, id: &str) -> Result<(), Error> {
        self.conversation.delete_session(id)
    }

    pub fn discard_if_empty(&mut self) -> Result<bool, Error> {
        self.conversation.discard_if_empty()
    }

    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        self.conversation.load_session(id)
    }

    /// Reverts the last user-initiated turn.
    ///
    /// # Errors
    ///
    /// Returns session or checkpoint I/O errors.
    pub fn undo_last_turn(&mut self) -> Result<UndoReport, Error> {
        self.conversation.undo_last_turn()
    }

    /// Every path `write_file`/`edit_file` touched this session, each with its
    /// oldest pre-image.
    pub fn touched_files(&self) -> Vec<crate::checkpoint::TouchedFile> {
        self.conversation.touched_files()
    }

    pub fn scan_tools(&mut self) -> Registry {
        self.conversation.scan_tools()
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        self.conversation.change_global_config(change)
    }

    pub(crate) fn change_global_config_batch(
        &mut self,
        changes: Vec<ConfigChange>,
    ) -> Result<Vec<ConfigChangeEffect>, Error> {
        self.conversation.change_global_config_batch(changes)
    }

    pub fn run_turn(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation.run_turn(user_input, sink)
    }

    /// Runs a turn with text and optional inline images.
    pub fn run_turn_input(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation.run_turn_input(user_input, sink)
    }

    pub(crate) fn run_turn_input_preserving_cancellation(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_turn_input_preserving_cancellation(user_input, sink)
    }

    pub(crate) fn run_init_preserving_cancellation(
        &mut self,
        input: TurnInput,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_init_preserving_cancellation(input, sink)
    }

    pub(crate) fn run_goal_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation.run_goal_preserving_cancellation(sink)
    }

    pub(crate) fn run_plan_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation.run_plan_preserving_cancellation(sink)
    }

    pub(crate) fn run_plan_follow_up_preserving_cancellation(
        &mut self,
        input: TurnInput,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_plan_follow_up_preserving_cancellation(input, sink)
    }

    pub(crate) fn run_plan_implementation_preserving_cancellation(
        &mut self,
        input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_plan_implementation_preserving_cancellation(input, sink)
    }

    pub(crate) fn has_deferred_subagent_results(&self) -> bool {
        self.subagents().has_deferred()
    }

    pub(crate) fn run_deferred_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Option<bool>, Error> {
        self.conversation.run_deferred_subagent_results(sink)
    }

    /// Delivers settled subagent results and waits for still-running ones
    /// until nothing is pending. Follow-up turns stream through `sink`.
    /// Returns `false` when the pump was interrupted.
    pub fn pump_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        wait_slice_secs: u64,
    ) -> Result<bool, Error> {
        self.conversation
            .pump_subagent_results(sink, wait_slice_secs)
    }

    pub fn compact_now(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        self.conversation.compact_now(sink)
    }

    pub(crate) fn compact_now_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        self.conversation.compact_now_preserving_cancellation(sink)
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.subagents().shutdown_and_discard();
        self.background_processes().shutdown_and_discard();
    }
}
