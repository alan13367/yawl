use crate::compaction;
use crate::config::Config;
use crate::error::Error;
use crate::provider::{
    self, Message, MessageControl, SubagentResult, ToolCall, TurnInput, stream_turn,
};
use crate::tools::Registry;

use super::goal;
use super::plan::{self, FollowUpAction};
use super::{Conversation, ConversationKind, last_undoable_user_index};
use crate::agent::events::{TurnEvent, forward};

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnMode {
    Normal,
    Goal,
    Plan,
    PlanFollowUp,
    PlanRevise,
    PlanImplement,
}

fn plan_prompt(
    state: Option<&crate::session::PlanState>,
    mode: TurnMode,
) -> Option<crate::prompt::PlanPrompt<'_>> {
    use crate::prompt::PlanPrompt;
    use crate::session::PlanState;
    match (mode, state) {
        (TurnMode::Plan, Some(PlanState::Draft { objective, .. })) => {
            Some(PlanPrompt::Draft(objective))
        }
        (TurnMode::PlanRevise, Some(PlanState::Ready { plan })) => Some(PlanPrompt::Revise(plan)),
        (TurnMode::PlanFollowUp, Some(PlanState::Ready { plan })) => {
            Some(PlanPrompt::FollowUp(plan))
        }
        (TurnMode::PlanImplement, Some(PlanState::Ready { plan })) => {
            Some(PlanPrompt::Implement(plan))
        }
        (_, Some(PlanState::Ready { plan })) => Some(PlanPrompt::Active(plan)),
        _ => None,
    }
}

fn plan_continuation(mode: TurnMode) -> Option<&'static str> {
    match mode {
        TurnMode::Plan | TurnMode::PlanRevise => Some(plan::PLAN_CONTINUATION),
        TurnMode::PlanImplement => Some(plan::PLAN_IMPLEMENT_CONTINUATION),
        // Follow-up turns end on a text reply so the model can answer
        // requests unrelated to the plan without being forced to classify.
        TurnMode::Normal | TurnMode::Goal | TurnMode::PlanFollowUp => None,
    }
}

impl Conversation {
    /// Runs one full turn. `user_input` is `None` when re-driving an existing
    /// conversation (not used by the current front ends, but harmless).
    ///
    /// Returns `Ok(true)` if the turn completed, `Ok(false)` if it was
    /// aborted by Ctrl+C.
    pub fn run_turn(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_turn_input(user_input.map(Into::into), sink)
    }

    pub fn run_turn_input(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.cancellation.clear();
            self.run_turn_input_with_mode(
                user_input,
                sink,
                &mut provider::resolve,
                TurnMode::Normal,
            )
        })
    }

    pub(crate) fn run_turn_preserving_cancellation(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_turn_input_preserving_cancellation(user_input.map(Into::into), sink)
    }

    pub(crate) fn run_turn_input_preserving_cancellation(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            let Some(timeout) = self.run_limits().and_then(|limits| limits.timeout) else {
                return self.run_turn_input_with_mode(
                    user_input,
                    sink,
                    &mut provider::resolve,
                    TurnMode::Normal,
                );
            };
            let (result, timed_out) =
                crate::cancellation::with_timeout(&cancellation, timeout, || {
                    self.run_turn_input_with_mode(
                        user_input,
                        sink,
                        &mut provider::resolve,
                        TurnMode::Normal,
                    )
                });
            if timed_out {
                sink(TurnEvent::Warning("subagent timeout exceeded".into()));
            }
            result
        })
    }

    pub(crate) fn run_goal_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_input_with_mode(None, sink, &mut provider::resolve, TurnMode::Goal)
        })
    }

    pub(crate) fn run_plan_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_input_with_mode(None, sink, &mut provider::resolve, TurnMode::Plan)
        })
    }

    pub(crate) fn run_plan_follow_up_preserving_cancellation(
        &mut self,
        input: TurnInput,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_input_with_mode(
                Some(input),
                sink,
                &mut provider::resolve,
                TurnMode::PlanFollowUp,
            )
        })
    }

    pub(crate) fn run_plan_implementation_preserving_cancellation(
        &mut self,
        input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_input_with_mode(
                input,
                sink,
                &mut provider::resolve,
                TurnMode::PlanImplement,
            )
        })
    }

    /// Delivers settled subagent results and waits for still-running ones
    /// until nothing is pending. Blocking waits happen in `wait_slice_secs`
    /// slices so Ctrl+C stays responsive. Returns `false` when the pump was
    /// interrupted. Used by print mode, which has no event loop.
    pub(crate) fn pump_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        wait_slice_secs: u64,
    ) -> Result<bool, Error> {
        self.pump_subagent_results_with(sink, wait_slice_secs, &mut provider::resolve)
    }

    pub(crate) fn pump_subagent_results_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        wait_slice_secs: u64,
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        loop {
            if crate::cancellation::interrupted() {
                return Ok(false);
            }
            let Some(manager) = self.persistent_subagents() else {
                return Ok(true);
            };
            if manager.has_deferred() {
                if self.run_deferred_subagent_results_with(sink, resolve_provider)? == Some(false) {
                    return Ok(false);
                }
                continue;
            }
            let active = manager.active_count();
            if active == 0 {
                return Ok(true);
            }
            if self.print_mode {
                let noun = if active == 1 { "subagent" } else { "subagents" };
                eprintln!("waiting for {active} {noun}…");
            }
            // False means the slice timed out or the process was
            // interrupted; the loop top sorts out which.
            let _ = manager.wait_all(wait_slice_secs);
        }
    }

    /// Appends one synthetic user message carrying the drained deferred
    /// results and runs a follow-up turn on it. Results are restored if the
    /// append fails.
    pub(crate) fn run_deferred_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Option<bool>, Error> {
        self.run_deferred_subagent_results_with(sink, &mut provider::resolve)
    }

    pub(crate) fn run_deferred_subagent_results_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<Option<bool>, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        let manager = self
            .persistent_subagents()
            .expect("deferred results require a subagent manager");
        let deliveries = manager.drain_deferred();
        if deliveries.is_empty() {
            return Ok(None);
        }
        let backup = deliveries.clone();
        let mut results = Vec::new();
        for delivery in deliveries {
            let status = match delivery.outcome {
                crate::subagent::RunOutcome::Completed => "completed",
                crate::subagent::RunOutcome::Failed => "failed",
                crate::subagent::RunOutcome::Interrupted => "interrupted",
            };
            let content = if delivery.error.is_empty() {
                delivery.result
            } else if delivery.result.is_empty() {
                delivery.error
            } else {
                format!("{}\n\nError: {}", delivery.result, delivery.error)
            };
            results.push(SubagentResult {
                id: delivery.id.to_string(),
                name: delivery.name,
                status: status.into(),
                run_number: delivery.run_number,
                content,
            });
        }
        if let Err(error) = self.append_input_message(Message::subagent_results(results)) {
            manager.restore_deferred(backup);
            return Err(error);
        }
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.run_turn_with(None, sink, resolve_provider)
        })
        .map(Some)
    }

    pub(super) fn run_turn_with<F>(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with(user_input.map(Into::into), sink, resolve_provider)
    }

    pub(super) fn run_turn_input_with<F>(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with_mode(user_input, sink, resolve_provider, TurnMode::Normal)
    }

    #[cfg(test)]
    pub(super) fn run_goal_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with_mode(None, sink, resolve_provider, TurnMode::Goal)
    }

    #[cfg(test)]
    pub(super) fn run_plan_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with_mode(None, sink, resolve_provider, TurnMode::Plan)
    }

    #[cfg(test)]
    pub(super) fn run_plan_follow_up_with<F>(
        &mut self,
        input: TurnInput,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with_mode(Some(input), sink, resolve_provider, TurnMode::PlanFollowUp)
    }

    #[cfg(test)]
    pub(super) fn run_plan_implementation_with<F>(
        &mut self,
        input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.run_turn_input_with_mode(input, sink, resolve_provider, TurnMode::PlanImplement)
    }

    fn run_turn_input_with_mode<F>(
        &mut self,
        user_input: Option<TurnInput>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
        mut mode: TurnMode,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        if mode == TurnMode::Goal && self.active_goal().is_none() {
            return Err(Error::Config("no active goal to resume".into()));
        }
        match mode {
            TurnMode::Plan
                if !matches!(
                    self.plan_state(),
                    Some(crate::session::PlanState::Draft { .. })
                ) =>
            {
                return Err(Error::Config("no draft plan to resume".into()));
            }
            TurnMode::PlanFollowUp | TurnMode::PlanImplement if self.active_plan().is_none() => {
                return Err(Error::Config("no completed plan is active".into()));
            }
            _ => {}
        }
        self.questions.begin_turn();
        self.plan_ready_this_turn = false;
        self.latest_turn_result.clear();
        if mode == TurnMode::PlanImplement && user_input.is_none() {
            if let Some(Err(error)) = self.checkpoint_snapshot() {
                sink(TurnEvent::Warning(format!(
                    "Could not checkpoint for /undo: {error}"
                )));
            }
            self.append_input_message(
                Message::user("Implement the active plan now.")
                    .with_control(MessageControl::PlanImplementationStart),
            )?;
        }
        if let Some(input) = user_input {
            if !input.images.is_empty() && !crate::model::supports_images(&self.config, &self.model)
            {
                return Err(Error::Config(format!(
                    "model '{}' does not accept image input",
                    self.model
                )));
            }
            if let Some(Err(error)) = self.checkpoint_snapshot() {
                sink(TurnEvent::Warning(format!(
                    "Could not checkpoint for /undo: {error}"
                )));
            }
            self.append_input_message(Message::user_input(input))?;
        }
        // Per-run guard rails: only subagent conversations carry limits.
        let limits = self.run_limits();
        let max_requests = limits.map_or(0, |limits| limits.max_requests);
        let hard_requests = max_requests.saturating_add((max_requests / 2).max(1));
        let mut requests_made: u64 = 0;
        let mut steer_sent = false;
        let mut asked_plan_questions = mode == TurnMode::Plan
            && self
                .plan_state()
                .is_some_and(crate::session::PlanState::questions_asked);

        // Uncapped: the loop ends when the model stops calling tools, or
        // when a goal completes through goal_complete.
        loop {
            if crate::cancellation::interrupted() {
                return Ok(false);
            }
            if matches!(mode, TurnMode::Goal | TurnMode::PlanImplement) {
                self.drain_deferred_subagent_results_into_history()?;
            }
            if max_requests > 0 {
                if requests_made == max_requests && !steer_sent {
                    steer_sent = true;
                    sink(TurnEvent::Warning(
                        "subagent request budget reached; steering to wrap up".into(),
                    ));
                    self.append_input_message(Message::user(
                        "Request budget reached. Finish your current step and reply with your final result now; do not start new work.",
                    ))?;
                }
                if requests_made >= hard_requests {
                    sink(TurnEvent::Warning(
                        "subagent request budget exceeded".into(),
                    ));
                    self.cancellation.cancel();
                    return Ok(false);
                }
            }
            // Rescan every iteration so a tool the model just wrote is
            // available on its very next step.
            let mut registry = self.scan_tools();
            match mode {
                TurnMode::Goal => registry.advertise_goal_complete(),
                TurnMode::Plan | TurnMode::PlanRevise => {
                    registry.retain_for_planning();
                    registry.advertise_plan_complete();
                }
                TurnMode::PlanFollowUp => {
                    registry.retain_for_planning();
                    registry.advertise_plan_action();
                }
                TurnMode::PlanImplement => registry.advertise_plan_implemented(),
                TurnMode::Normal => {}
            }
            let specs = registry.specs();
            let system = match &self.kind {
                ConversationKind::Persistent(state) => crate::prompt::build_system_prompt(
                    &self.config.home_dir,
                    self.config.subagents,
                    self.print_mode,
                    registry.has_web_tools(),
                    registry.skills(),
                    crate::prompt::MainPromptState {
                        goal: (mode == TurnMode::Goal)
                            .then_some(state.active_goal.as_deref())
                            .flatten(),
                        plan: plan_prompt(state.session.active_plan(), mode),
                        interactive_questions: self.questions.is_enabled(),
                    },
                ),
                ConversationKind::Child(_) => crate::prompt::build_subagent_system_prompt(
                    &self.config.home_dir,
                    self.role_fragment(),
                    registry.has_web_tools(),
                    registry.skills(),
                ),
            };

            self.maybe_compact(sink, resolve_provider)?;

            let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
            let request = provider::Request {
                model: &bare_model,
                system: &system,
                messages: &self.messages,
                tools: &specs,
                max_tokens: crate::model::max_tokens(&self.config, &self.model),
                supports_images: crate::model::supports_images(&self.config, &self.model),
                prompt_cache_control: true,
                prompt_cache_key: Some(self.prompt_cache_key()),
            };
            let out = match stream_turn(provider.as_ref(), &request, &mut forward(sink)) {
                Ok(out) => out,
                // Abort quietly: partial output is discarded, history stays
                // valid (it still ends with a user/tool message).
                Err(Error::Interrupted) => return Ok(false),
                Err(e) => return Err(e),
            };
            if crate::cancellation::interrupted() {
                return Ok(false);
            }

            self.record_usage(out.usage)?;
            self.context_tokens = out.usage.total_tokens();
            requests_made = requests_made.saturating_add(1);
            sink(TurnEvent::Usage {
                context_tokens: self.context_tokens,
                context_window: self.context_window(),
                request_usage: out.usage,
                session_usage: self.usage(),
            });

            self.latest_turn_result.clone_from(&out.text);
            let mut assistant = Message::assistant(out.text, out.tool_calls.clone());
            assistant.reasoning = out.reasoning;
            assistant.provider_data = out.provider_data;
            if !assistant.provider_data.is_empty() {
                assistant.provider_data_model = Some(bare_model.clone());
            }

            if self.steers.has_pending() {
                self.persist_message(&assistant)?;
                self.messages.push(assistant);
                sink(TurnEvent::AssistantDone);
                self.skip_tool_calls(&out.tool_calls, goal::STEER_SKIPPED)?;
                if self.inject_pending_steers(sink)? || !out.tool_calls.is_empty() {
                    continue;
                }
                if mode == TurnMode::Goal {
                    self.append_goal_continuation()?;
                    continue;
                }
                if let Some(continuation) = plan_continuation(mode) {
                    self.append_plan_continuation(continuation)?;
                    continue;
                }
                return Ok(true);
            }

            if out.tool_calls.iter().any(plan::is_user_input) {
                self.persist_assistant(assistant, sink)?;
                let question_validation = crate::tools::validate_user_input(
                    out.tool_calls
                        .first()
                        .map_or("{}", |call| call.arguments.as_str()),
                );
                let valid_batch = out.tool_calls.len() == 1
                    && (!matches!(mode, TurnMode::Plan | TurnMode::PlanRevise)
                        || question_validation == Ok(3));
                if !valid_batch {
                    let error = if out.tool_calls.len() != 1 {
                        "request_user_input must be the only tool call in its model step"
                    } else if let Err(error) = &question_validation {
                        error
                    } else {
                        "planning and revision require exactly three questions per request_user_input call"
                    };
                    self.reject_tool_calls(&out.tool_calls, error)?;
                    continue;
                }
                let aborted = self.run_tools(&registry, &out.tool_calls, sink)?;
                if aborted {
                    return Ok(false);
                }
                if matches!(mode, TurnMode::Plan | TurnMode::PlanRevise) {
                    asked_plan_questions = true;
                    if mode == TurnMode::Plan {
                        self.persistent_mut()
                            .session
                            .append_plan_questions_asked()?;
                    }
                }
                continue;
            }

            let has_private_plan_call = out.tool_calls.iter().any(|call| {
                plan::is_plan_complete(call)
                    || plan::is_plan_action(call)
                    || plan::is_plan_implemented(call)
            });
            if has_private_plan_call {
                if out.tool_calls.len() != 1 {
                    self.persist_assistant(assistant, sink)?;
                    self.reject_tool_calls(
                        &out.tool_calls,
                        "private planning tools must be the only tool call in their model step",
                    )?;
                    continue;
                }
                let call = &out.tool_calls[0];
                match mode {
                    TurnMode::Plan | TurnMode::PlanRevise if plan::is_plan_complete(call) => {
                        if !asked_plan_questions {
                            self.persist_assistant(assistant, sink)?;
                            self.reject_tool_calls(&out.tool_calls, plan::PLAN_QUESTION_REQUIRED)?;
                            continue;
                        }
                        match plan::parse_plan(&call.arguments) {
                            Ok(result) => {
                                self.finish_plan_complete(assistant, result, sink)?;
                                return Ok(true);
                            }
                            Err(error) => {
                                self.persist_assistant(assistant, sink)?;
                                self.reject_tool_calls(&out.tool_calls, &error)?;
                            }
                        }
                        continue;
                    }
                    TurnMode::PlanFollowUp if plan::is_plan_action(call) => {
                        match plan::parse_action(&call.arguments) {
                            Ok(action) => {
                                self.persist_assistant(assistant, sink)?;
                                if self.run_tools(&registry, &out.tool_calls, sink)? {
                                    return Ok(false);
                                }
                                mode = match action {
                                    FollowUpAction::Revise => TurnMode::PlanRevise,
                                    FollowUpAction::Implement => TurnMode::PlanImplement,
                                    FollowUpAction::Unrelated => TurnMode::Normal,
                                };
                                asked_plan_questions = false;
                            }
                            Err(error) => {
                                self.persist_assistant(assistant, sink)?;
                                self.reject_tool_calls(&out.tool_calls, &error)?;
                            }
                        }
                        continue;
                    }
                    TurnMode::PlanImplement if plan::is_plan_implemented(call) => {
                        match plan::parse_implemented(&call.arguments) {
                            Ok(result) => {
                                self.finish_plan_implemented(assistant, result, sink)?;
                                return Ok(true);
                            }
                            Err(error) => {
                                self.persist_assistant(assistant, sink)?;
                                self.reject_tool_calls(&out.tool_calls, &error)?;
                            }
                        }
                        continue;
                    }
                    _ => {
                        self.persist_assistant(assistant, sink)?;
                        self.reject_tool_calls(
                            &out.tool_calls,
                            "that planning tool is unavailable in the current phase",
                        )?;
                        continue;
                    }
                }
            }

            let (completes, ordinary) = if mode == TurnMode::Goal {
                goal::split_goal_complete(&out.tool_calls)
            } else {
                (Vec::new(), out.tool_calls.iter().collect())
            };
            if completes.len() == 1 && ordinary.is_empty() {
                match goal::parse_goal_complete_result(&completes[0].arguments) {
                    Ok(result) => {
                        self.finish_goal_complete(assistant, result, sink)?;
                        return Ok(true);
                    }
                    Err(error) => {
                        self.persist_assistant(assistant, sink)?;
                        self.reject_goal_complete(completes[0], &error)?;
                    }
                }
                continue;
            }
            if !completes.is_empty() {
                self.persist_assistant(assistant, sink)?;
                let aborted = self.run_tools_rejecting_goal_complete(
                    &registry,
                    &out.tool_calls,
                    sink,
                    "goal_complete must be the only tool call in its step",
                )?;
                if aborted {
                    return Ok(false);
                }
                if self.inject_pending_steers(sink)? {
                    continue;
                }
                continue;
            }

            self.persist_assistant(assistant, sink)?;
            if out.tool_calls.is_empty() {
                if self.inject_pending_steers(sink)? {
                    continue;
                }
                if mode == TurnMode::Goal {
                    self.append_goal_continuation()?;
                    continue;
                }
                if let Some(continuation) = plan_continuation(mode) {
                    self.append_plan_continuation(continuation)?;
                    continue;
                }
                return Ok(true);
            }
            let aborted = self.run_tools(&registry, &out.tool_calls, sink)?;
            if aborted {
                return Ok(false);
            }
            if self.inject_pending_steers(sink)? {
                continue;
            }
        }
    }

    /// Executes tool calls in order. On interrupt, remaining calls get
    /// synthetic error results so every tool call keeps a paired result and
    /// the history stays valid for the next request.
    fn run_tools(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_tools_while_inner(
            registry,
            calls,
            sink,
            crate::cancellation::interrupted,
            None,
        )
    }

    fn run_tools_rejecting_goal_complete(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
        error: &str,
    ) -> Result<bool, Error> {
        self.run_tools_while_inner(
            registry,
            calls,
            sink,
            crate::cancellation::interrupted,
            Some(error),
        )
    }

    #[cfg(test)]
    pub(super) fn run_tools_while(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
        mut interrupted: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        self.run_tools_while_inner(registry, calls, sink, &mut interrupted, None)
    }

    fn run_tools_while_inner(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
        mut interrupted: impl FnMut() -> bool,
        goal_complete_error: Option<&str>,
    ) -> Result<bool, Error> {
        let mut aborted = false;
        let mut skip_remaining = false;
        for call in calls {
            let result = if aborted {
                Message::tool_result(
                    &call.id,
                    &call.name,
                    "[interrupted by user]".to_string(),
                    true,
                )
            } else if skip_remaining {
                Message::tool_result(&call.id, &call.name, goal::STEER_SKIPPED.to_string(), true)
                    .with_control(MessageControl::ToolSkipped)
            } else if goal_complete_error.is_some() && goal::is_goal_complete(call) {
                Message::tool_result(
                    &call.id,
                    &call.name,
                    goal_complete_error.unwrap_or_default().to_string(),
                    true,
                )
            } else if interrupted() {
                aborted = true;
                Message::tool_result(
                    &call.id,
                    &call.name,
                    "[interrupted by user]".to_string(),
                    true,
                )
            } else {
                sink(TurnEvent::ToolStart {
                    name: &call.name,
                    args: &call.arguments,
                });
                if let Some(path) =
                    crate::checkpoint::mutating_tool_path(&call.name, &call.arguments)
                    && let Some(Err(error)) = self.checkpoint_path(&path)
                {
                    sink(TurnEvent::Warning(format!(
                        "Could not record {} for /undo: {error}",
                        path.display()
                    )));
                }
                let outcome = registry.execute_with_capabilities(
                    &call.name,
                    &call.arguments,
                    self.session_id(),
                    crate::model::supports_images(&self.config, &self.model),
                );
                sink(TurnEvent::ToolEnd {
                    name: &call.name,
                    output: &outcome.content,
                    images: &outcome.images,
                    is_error: outcome.is_error,
                });
                if interrupted() {
                    aborted = true;
                } else if self.steers.has_pending() {
                    skip_remaining = true;
                }
                Message::tool_result_with_images(
                    &call.id,
                    &call.name,
                    outcome.content,
                    outcome.images,
                    outcome.is_error,
                )
            };
            self.persist_message(&result)?;
            self.messages.push(result);
        }
        Ok(aborted)
    }

    fn persist_assistant(
        &mut self,
        assistant: Message,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        self.persist_message(&assistant)?;
        self.messages.push(assistant);
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn skip_tool_calls(&mut self, calls: &[ToolCall], reason: &str) -> Result<(), Error> {
        for call in calls {
            let result = Message::tool_result(&call.id, &call.name, reason.to_string(), true)
                .with_control(MessageControl::ToolSkipped);
            self.persist_message(&result)?;
            self.messages.push(result);
        }
        Ok(())
    }

    fn reject_goal_complete(&mut self, call: &ToolCall, error: &str) -> Result<(), Error> {
        let result = Message::tool_result(&call.id, &call.name, error.to_string(), true);
        self.persist_message(&result)?;
        self.messages.push(result);
        Ok(())
    }

    fn reject_tool_calls(&mut self, calls: &[ToolCall], error: &str) -> Result<(), Error> {
        for call in calls {
            let result = Message::tool_result(&call.id, &call.name, error.to_string(), true);
            self.persist_message(&result)?;
            self.messages.push(result);
        }
        Ok(())
    }

    fn inject_pending_steers(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let mut steers = std::collections::VecDeque::from(self.steers.drain());
        if steers.is_empty() {
            return Ok(false);
        }
        let mut accepted = false;
        while let Some(input) = steers.pop_front() {
            if !input.images.is_empty() && !crate::model::supports_images(&self.config, &self.model)
            {
                sink(TurnEvent::Warning(format!(
                    "model '{}' does not accept image input",
                    self.model
                )));
                continue;
            }
            let message = Message::user_input(input.clone()).with_control(MessageControl::Steering);
            let text = message.content.clone();
            if let Err(error) = self.append_input_message(message) {
                let pending = std::iter::once(input).chain(steers).collect::<Vec<_>>();
                self.steers.prepend(pending);
                return Err(error);
            }
            sink(TurnEvent::SteerAccepted { text: &text });
            accepted = true;
        }
        Ok(accepted)
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
        if assistant.content.is_empty() {
            sink(TurnEvent::TextDelta(&result));
        } else if assistant.content != result {
            sink(TurnEvent::AssistantReplace(&result));
        }
        assistant.content.clone_from(&result);
        assistant.tool_calls.clear();
        self.persistent_mut()
            .session
            .append_goal_complete(&assistant)?;
        self.messages.push(assistant);
        self.latest_turn_result.clone_from(&result);
        self.persistent_mut().active_goal = None;
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn finish_plan_complete(
        &mut self,
        mut assistant: Message,
        plan: String,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        if assistant.content.is_empty() {
            sink(TurnEvent::TextDelta(&plan));
        } else if assistant.content != plan {
            sink(TurnEvent::AssistantReplace(&plan));
        }
        assistant.content.clone_from(&plan);
        assistant.tool_calls.clear();
        self.persistent_mut()
            .session
            .append_plan_ready(&plan, &assistant)?;
        self.messages.push(assistant);
        self.latest_turn_result.clone_from(&plan);
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
        if assistant.content.is_empty() {
            sink(TurnEvent::TextDelta(&result));
        } else if assistant.content != result {
            sink(TurnEvent::AssistantReplace(&result));
        }
        assistant.content.clone_from(&result);
        assistant.tool_calls.clear();
        self.persistent_mut()
            .session
            .append_plan_implemented(&assistant)?;
        self.messages.push(assistant);
        self.latest_turn_result.clone_from(&result);
        sink(TurnEvent::AssistantDone);
        Ok(())
    }

    fn drain_deferred_subagent_results_into_history(&mut self) -> Result<(), Error> {
        let Some(manager) = self.persistent_subagents() else {
            return Ok(());
        };
        if !manager.has_deferred() {
            return Ok(());
        }
        let deliveries = manager.drain_deferred();
        if deliveries.is_empty() {
            return Ok(());
        }
        let backup = deliveries.clone();
        let mut results = Vec::new();
        for delivery in deliveries {
            let status = match delivery.outcome {
                crate::subagent::RunOutcome::Completed => "completed",
                crate::subagent::RunOutcome::Failed => "failed",
                crate::subagent::RunOutcome::Interrupted => "interrupted",
            };
            let content = if delivery.error.is_empty() {
                delivery.result
            } else if delivery.result.is_empty() {
                delivery.error
            } else {
                format!("{}\n\nError: {}", delivery.result, delivery.error)
            };
            results.push(SubagentResult {
                id: delivery.id.to_string(),
                name: delivery.name,
                status: status.into(),
                run_number: delivery.run_number,
                content,
            });
        }
        if let Err(error) = self.append_input_message(Message::subagent_results(results)) {
            manager.restore_deferred(backup);
            return Err(error);
        }
        Ok(())
    }

    fn maybe_compact<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        if !self.config.auto_compact
            || !compaction::should_compact(
                self.context_tokens,
                self.context_window(),
                self.config.compact_threshold,
            )
        {
            return Ok(());
        }
        match self.compact_now_with(sink, resolve_provider) {
            // A failed auto-compaction shouldn't kill the turn; the request
            // may still fit. Manual /compact reports errors directly.
            Err(Error::Interrupted) => Err(Error::Interrupted),
            Ok(()) => Ok(()),
            Err(error) => {
                sink(TurnEvent::Warning(format!(
                    "Auto-compaction failed; continuing without compacting: {error}"
                )));
                Ok(())
            }
        }
    }

    /// Summarizes the head of the conversation with the current model
    /// (also the `/compact` slash command).
    pub fn compact_now(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.cancellation.clear();
            self.compact_now_with(sink, &mut provider::resolve)
        })
    }

    pub(crate) fn compact_now_preserving_cancellation(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.compact_now_with(sink, &mut provider::resolve)
        })
    }

    pub(super) fn compact_now_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        sink(TurnEvent::Compacting);
        let registry = self.scan_tools();
        let specs = registry.specs();
        let system = match &self.kind {
            ConversationKind::Persistent(state) => crate::prompt::build_system_prompt(
                &self.config.home_dir,
                self.config.subagents,
                self.print_mode,
                registry.has_web_tools(),
                registry.skills(),
                crate::prompt::MainPromptState {
                    goal: state.active_goal.as_deref(),
                    plan: state
                        .session
                        .active_plan()
                        .and_then(crate::session::PlanState::ready)
                        .map(crate::prompt::PlanPrompt::Active),
                    interactive_questions: self.questions.is_enabled(),
                },
            ),
            ConversationKind::Child(_) => crate::prompt::build_subagent_system_prompt(
                &self.config.home_dir,
                self.role_fragment(),
                registry.has_web_tools(),
                registry.skills(),
            ),
        };
        let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
        let (summary, range, summary_usage) = compaction::summarize(
            provider.as_ref(),
            &bare_model,
            crate::model::max_tokens(&self.config, &self.model),
            &self.messages,
            last_undoable_user_index(&self.messages),
            // Summarizer output is not user-facing; swallow its deltas.
            &mut |_| {},
        )?;
        self.record_usage(summary_usage)?;

        let remote_request = provider::Request {
            model: &bare_model,
            system: &system,
            messages: &self.messages[range.clone()],
            tools: &specs,
            max_tokens: crate::model::max_tokens(&self.config, &self.model),
            supports_images: crate::model::supports_images(&self.config, &self.model),
            prompt_cache_control: true,
            prompt_cache_key: Some(self.prompt_cache_key()),
        };
        let remote = match provider.compact(&remote_request) {
            Ok(output) => output,
            Err(_) if crate::cancellation::interrupted() => return Err(Error::Interrupted),
            Err(error) => {
                sink(TurnEvent::Warning(format!(
                    "Codex remote compaction failed; using the portable summary: {error}"
                )));
                None
            }
        };
        let mut request_usage = summary_usage;
        let (provider_data, provider_data_model) = if let Some(remote) = remote {
            self.record_usage(remote.usage)?;
            request_usage = request_usage.saturating_add(remote.usage);
            (remote.replacement_history, Some(bare_model.clone()))
        } else {
            (Vec::new(), None)
        };

        let start = range.start;
        let replaced = range.len();
        self.persist_compaction(
            &summary,
            start,
            replaced,
            &provider_data,
            provider_data_model.as_deref(),
        )?;
        compaction::apply_summary_range_with_provider_data(
            &mut self.messages,
            &summary,
            range,
            provider_data,
            provider_data_model,
        );
        self.record_cache_reset()?;
        // Old usage estimate is stale after compaction; a fresh number
        // arrives with the next response.
        self.context_tokens = 0;
        sink(TurnEvent::Usage {
            context_tokens: 0,
            context_window: self.context_window(),
            request_usage,
            session_usage: self.usage(),
        });
        sink(TurnEvent::Compacted { replaced });
        Ok(())
    }
}
