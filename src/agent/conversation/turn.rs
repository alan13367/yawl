//! The turn lifecycle: entry points, the per-turn preamble, and the model
//! step loop. What happens with each response lives in [`super::step`].

use std::sync::Arc;

use crate::compaction;
use crate::config::Config;
use crate::error::Error;
use crate::provider::{
    self, Message, MessageControl, SubagentResult, ToolCall, ToolSpec, TurnInput, TurnOutput,
    stream_turn,
};
use crate::tools::Registry;

use super::step::{self, Flow};
use super::{Conversation, RunLimits, RunStopReason, last_undoable_user_index};
use super::{context, goal, plan};
use crate::agent::events::{PlanPhase, TurnEvent, forward};

/// Corrective retries after the server rejects an unparseable tool call.
const MALFORMED_TOOL_CALL_RETRIES: u32 = 2;
const MALFORMED_TOOL_CALL_NOTE: &str = "[yawl] Your previous response contained a tool call \
the server could not parse, so it was discarded and nothing ran. Continue the task: \
re-issue the call with a valid tool name and JSON arguments, or answer in plain text.";

/// Weaker models often invent tool names, so the retry note names the
/// tools that actually exist.
fn malformed_tool_call_note(specs: &[Arc<ToolSpec>]) -> String {
    if specs.is_empty() {
        return format!("{MALFORMED_TOOL_CALL_NOTE} No tools are available; answer in plain text.");
    }
    let names = specs
        .iter()
        .map(|spec| spec.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("{MALFORMED_TOOL_CALL_NOTE} Available tools: {names}.")
}

/// What one turn does. Every front end runs turns through
/// [`Conversation::run`].
pub(crate) enum TurnRequest {
    /// An ordinary turn. `None` re-drives the history without new input.
    Prompt(Option<TurnInput>),
    /// `/init`, with its literal prompt.
    Init(TurnInput),
    /// Continue the active goal.
    Goal,
    /// Continue drafting the active plan.
    Plan,
    /// A message while a plan is ready; the model routes it to revision,
    /// implementation, or a normal reply.
    PlanFollowUp(TurnInput),
    /// Revise the ready plan with this request.
    PlanRevise(TurnInput),
    /// Implement the ready plan, optionally with extra instructions.
    PlanImplement(Option<TurnInput>),
    /// Re-drive the last turn that stopped early, in the mode it stopped in.
    Continue,
}

impl TurnRequest {
    /// The input to record and the mode to run. `Continue` takes its mode
    /// from the history instead.
    fn into_input_and_mode(self) -> Option<(Option<TurnInput>, TurnMode)> {
        Some(match self {
            Self::Prompt(input) => (input, TurnMode::Normal),
            Self::Init(input) => (Some(input), TurnMode::Init),
            Self::Goal => (None, TurnMode::Goal),
            Self::Plan => (None, TurnMode::Plan),
            Self::PlanFollowUp(input) => (Some(input), TurnMode::PlanFollowUp),
            Self::PlanRevise(input) => (Some(input), TurnMode::PlanRevise),
            Self::PlanImplement(input) => (input, TurnMode::PlanImplement),
            Self::Continue => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnMode {
    Normal,
    Init,
    Goal,
    Plan,
    PlanFollowUp,
    PlanRevise,
    PlanImplement,
}

/// `implementation_started` is true once the ready plan was handed off to
/// implementation, so a follow-up can resume work that stopped early.
fn plan_prompt<'a>(
    state: Option<&'a crate::session::PlanState>,
    path: Option<&'a str>,
    mode: TurnMode,
    implementation_started: bool,
) -> Option<crate::prompt::PlanPrompt<'a>> {
    use crate::prompt::PlanPrompt;
    use crate::session::PlanState;
    match (mode, state) {
        (TurnMode::Plan, Some(PlanState::Draft { objective, .. })) => {
            Some(PlanPrompt::Draft(objective))
        }
        (_, Some(PlanState::Draft { objective, .. })) => Some(PlanPrompt::Paused(objective)),
        (TurnMode::PlanRevise, Some(PlanState::Ready { .. })) => path.map(PlanPrompt::Revise),
        (TurnMode::PlanFollowUp, Some(PlanState::Ready { .. })) if implementation_started => {
            path.map(PlanPrompt::ImplementationFollowUp)
        }
        (TurnMode::PlanFollowUp, Some(PlanState::Ready { .. })) => path.map(PlanPrompt::FollowUp),
        (TurnMode::PlanImplement, Some(PlanState::Ready { .. })) => path.map(PlanPrompt::Implement),
        (_, Some(PlanState::Ready { .. })) => path.map(PlanPrompt::Active),
        _ => None,
    }
}

fn plan_phase(mode: TurnMode) -> Option<PlanPhase> {
    match mode {
        TurnMode::Plan => Some(PlanPhase::Planning),
        TurnMode::PlanFollowUp => Some(PlanPhase::FollowUp),
        TurnMode::PlanRevise => Some(PlanPhase::Revising),
        TurnMode::PlanImplement => Some(PlanPhase::Implementing),
        TurnMode::Normal | TurnMode::Init | TurnMode::Goal => None,
    }
}

pub(super) fn plan_continuation(mode: TurnMode) -> Option<&'static str> {
    match mode {
        TurnMode::Plan => Some(plan::PLAN_CONTINUATION),
        TurnMode::PlanRevise => Some(plan::PLAN_REVISE_CONTINUATION),
        TurnMode::PlanImplement => Some(plan::PLAN_IMPLEMENT_CONTINUATION),
        // Follow-up turns end on a text reply so the model can answer
        // requests unrelated to the plan without being forced to classify.
        TurnMode::Normal | TurnMode::Init | TurnMode::Goal | TurnMode::PlanFollowUp => None,
    }
}

/// A subagent's request budget: a wrap-up steer once `max` requests have
/// completed and a hard stop half again later (at least one more). Zero
/// disables it.
struct RequestBudget {
    max: u64,
    made: u64,
    steered: bool,
}

impl RequestBudget {
    fn new(limits: Option<RunLimits>) -> Self {
        Self {
            max: limits.map_or(0, |limits| limits.max_requests),
            made: 0,
            steered: false,
        }
    }

    fn hard_limit(&self) -> u64 {
        self.max.saturating_add((self.max / 2).max(1))
    }
}

/// What a turn carries from one model step to the next.
pub(super) struct TurnProgress {
    /// A plan follow-up can move to revision, implementation, or a normal
    /// reply mid-turn.
    pub(super) mode: TurnMode,
    pub(super) asked_plan_questions: bool,
    pub(super) idle_plan_replies: u32,
    budget: RequestBudget,
    /// Front ends start every turn without a plan phase label.
    announced_phase: Option<PlanPhase>,
}

/// Everything one model request uses, rebuilt every step so a tool the
/// model just wrote is available on its very next step.
pub(super) struct Step {
    pub(super) registry: Registry,
    specs: Vec<Arc<ToolSpec>>,
    system: String,
    /// Estimated prompt tokens of the system prompt and tool schemas.
    overhead: u64,
    provider: Box<dyn provider::Provider>,
    bare_model: String,
}

impl Conversation {
    /// Runs one turn inside this conversation's cancellation scope. An
    /// earlier cancel is kept, so callers clear it when a new job starts.
    /// Subagent conversations also enforce their wall-clock limit.
    ///
    /// Returns `Ok(true)` if the turn completed and `Ok(false)` if it was
    /// interrupted or stopped by a limit.
    pub(crate) fn run(
        &mut self,
        request: TurnRequest,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.run_with(request, sink, &mut provider::resolve)
    }

    pub(super) fn run_with<F>(
        &mut self,
        request: TurnRequest,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.set_stop_reason(None);
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            let Some(timeout) = self.run_limits().and_then(|limits| limits.timeout) else {
                return self.run_request(request, sink, resolve_provider);
            };
            let (result, timed_out) =
                crate::cancellation::with_timeout(&cancellation, timeout, || {
                    self.run_request(request, sink, resolve_provider)
                });
            if timed_out {
                self.set_stop_reason(Some(RunStopReason::Timeout));
                sink(TurnEvent::Warning(RunStopReason::Timeout.message().into()));
            }
            result
        })
    }

    /// Runs the request and remembers whether it stopped short, so a failed
    /// or interrupted turn can be continued.
    fn run_request<F>(
        &mut self,
        request: TurnRequest,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        let (input, mode, resuming) = match request.into_input_and_mode() {
            Some((input, mode)) => (input, mode, false),
            None => {
                self.recover_history()?;
                let Some(mode) = self.continuable_mode() else {
                    return Err(Error::Invalid("nothing to continue".into()));
                };
                (None, mode, true)
            }
        };
        let result = self.drive_turn(input, resuming, mode, sink, resolve_provider);
        if matches!(result, Ok(true)) {
            self.unfinished_mode = None;
        }
        result
    }

    /// Whether the last turn stopped before the model finished answering,
    /// so [`TurnRequest::Continue`] can pick it up.
    pub(crate) fn can_continue(&self) -> bool {
        self.continuable_mode().is_some()
    }

    fn continuable_mode(&self) -> Option<TurnMode> {
        // A trailing user message or tool result, or a tool call whose result
        // `recover_history` will repair, still owes the model a response.
        let awaiting_model = self.messages().last().is_some_and(|message| {
            message.role != provider::Role::Assistant || !message.tool_calls.is_empty()
        });
        match self.unfinished_mode {
            // Goal and draft-plan turns re-enter through their own resume
            // logic, whatever the transcript ends with.
            Some(TurnMode::Goal) if self.active_goal().is_some() => Some(TurnMode::Goal),
            Some(TurnMode::Plan)
                if matches!(
                    self.plan_state(),
                    Some(crate::session::PlanState::Draft { .. })
                ) =>
            {
                Some(TurnMode::Plan)
            }
            Some(mode) if awaiting_model => Some(mode),
            // Sessions resumed from disk carry no mode; a dangling request
            // continues as a normal turn.
            None if awaiting_model => Some(TurnMode::Normal),
            _ => None,
        }
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

    pub(super) fn pump_subagent_results_with<F>(
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
            let manager = self.services.subagents.clone();
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
            sink(TurnEvent::WaitingForSubagents { active });
            // False means the slice timed out or the process was
            // interrupted; the loop top sorts out which.
            let _ = manager.wait_all(wait_slice_secs);
        }
    }

    /// Appends one synthetic user message carrying the drained deferred
    /// results and runs a follow-up turn on it. Results are restored if the
    /// append fails. `None` means nothing was waiting.
    pub(crate) fn run_deferred_subagent_results(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Option<bool>, Error> {
        self.run_deferred_subagent_results_with(sink, &mut provider::resolve)
    }

    pub(super) fn run_deferred_subagent_results_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<Option<bool>, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.recover_history()?;
        if !self.drain_deferred_subagent_results_into_history()? {
            return Ok(None);
        }
        self.run_with(TurnRequest::Prompt(None), sink, resolve_provider)
            .map(Some)
    }

    fn drive_turn<F>(
        &mut self,
        input: Option<TurnInput>,
        resuming: bool,
        mode: TurnMode,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.recover_history()?;
        self.check_turn_mode(mode)?;
        self.questions.begin_turn();
        self.plan_ready_this_turn = false;
        self.latest_turn_result.clear();
        if !resuming {
            self.append_turn_input(input, mode, sink)?;
        }
        let mut progress = TurnProgress {
            mode,
            asked_plan_questions: mode == TurnMode::Plan
                && self
                    .plan_state()
                    .is_some_and(crate::session::PlanState::questions_asked),
            idle_plan_replies: 0,
            budget: RequestBudget::new(self.run_limits()),
            announced_phase: None,
        };
        // Uncapped: the loop ends when the model stops calling tools, when a
        // finishing tool completes the turn, or when the user interrupts.
        loop {
            self.unfinished_mode = Some(progress.mode);
            if crate::cancellation::interrupted() {
                return Ok(false);
            }
            if let Some(done) = self.begin_step(&mut progress, sink)? {
                return Ok(done);
            }
            // Steers wait for the next request, so tool calls the model
            // already made run first instead of wasting their output.
            self.inject_pending_steers(sink)?;
            let step = self.prepare_step(progress.mode, sink, resolve_provider)?;
            let Some(out) = self.request_step(&step, sink, resolve_provider)? else {
                return Ok(false);
            };
            let (assistant, calls) = self.record_step(out, &step, &mut progress, sink)?;
            let kind = step::classify(progress.mode, &calls);
            match self.handle_step(kind, assistant, &calls, &step, &mut progress, sink)? {
                Flow::Continue => {}
                Flow::Finish(done) => return Ok(done),
            }
        }
    }

    fn check_turn_mode(&self, mode: TurnMode) -> Result<(), Error> {
        match mode {
            TurnMode::Goal if self.active_goal().is_none() => {
                Err(Error::Invalid("no active goal to resume".into()))
            }
            TurnMode::Plan
                if !matches!(
                    self.plan_state(),
                    Some(crate::session::PlanState::Draft { .. })
                ) =>
            {
                Err(Error::Invalid("no draft plan to resume".into()))
            }
            TurnMode::PlanFollowUp | TurnMode::PlanRevise | TurnMode::PlanImplement
                if self.active_plan().is_none() =>
            {
                Err(Error::Invalid("no completed plan is active".into()))
            }
            _ => Ok(()),
        }
    }

    /// Records the turn's new input after an undo checkpoint. Implementing
    /// without input records the hidden prompt that starts implementation.
    fn append_turn_input(
        &mut self,
        input: Option<TurnInput>,
        mode: TurnMode,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(), Error> {
        let message = match input {
            Some(input) => {
                if !input.images.is_empty()
                    && !crate::model::supports_images(&self.config, &self.model)
                {
                    return Err(Error::Invalid(format!(
                        "model '{}' does not accept image input",
                        self.model
                    )));
                }
                Message::user_input(input)
            }
            None if mode == TurnMode::PlanImplement => {
                Message::user("Implement the active plan now.")
                    .with_control(MessageControl::PlanImplementationStart)
            }
            None => return Ok(()),
        };
        if let Some(warning) = self.checkpoint_warning() {
            sink(TurnEvent::Warning(warning));
        }
        self.append_input_message(message)
    }

    /// Work before each model request: the phase label, deferred subagent
    /// results, the request budget, the plan handoff, and the question
    /// countdown. `Some` ends the turn.
    fn begin_step(
        &mut self,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<Option<bool>, Error> {
        let mode = progress.mode;
        let phase = plan_phase(mode);
        if phase != progress.announced_phase {
            progress.announced_phase = phase;
            sink(TurnEvent::PlanPhase(phase));
        }
        if matches!(mode, TurnMode::Goal | TurnMode::PlanImplement) {
            self.drain_deferred_subagent_results_into_history()?;
        }
        if self.enforce_budget(&mut progress.budget, sink)? {
            return Ok(Some(false));
        }
        if mode == TurnMode::PlanImplement {
            self.prepare_plan_handoff(sink)?;
        }
        // Planning decisions wait for the user; other modes auto-answer.
        self.questions
            .set_countdown(!matches!(mode, TurnMode::Plan | TurnMode::PlanRevise));
        Ok(None)
    }

    /// Steers a subagent to wrap up at its budget and stops it at the hard
    /// limit. Returns whether the run was stopped.
    fn enforce_budget(
        &mut self,
        budget: &mut RequestBudget,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        if budget.max == 0 {
            return Ok(false);
        }
        if budget.made == budget.max && !budget.steered {
            budget.steered = true;
            sink(TurnEvent::Warning(
                "subagent request budget reached; steering to wrap up".into(),
            ));
            self.append_input_message(Message::user(
                "Request budget reached. Finish your current step and reply with your final result now; do not start new work.",
            ))?;
        }
        if budget.made >= budget.hard_limit() {
            self.set_stop_reason(Some(RunStopReason::RequestBudget));
            sink(TurnEvent::Warning(
                RunStopReason::RequestBudget.message().into(),
            ));
            self.cancellation.cancel();
            return Ok(true);
        }
        Ok(false)
    }

    /// Builds the tools, prompt, and provider for the next request,
    /// compacting first when the context is nearly full. Plan phases share
    /// one tool list and system prompt so local servers keep their prompt
    /// cache; phases differ only in execution gates and a hidden note.
    fn prepare_step<F>(
        &mut self,
        mode: TurnMode,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<Step, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        let plan_path = self.plan_file_reference()?;
        let mut registry = self.scan_tools();
        self.report_catalog_warnings(&registry, sink);
        match mode {
            TurnMode::Goal => registry.advertise_goal_complete(),
            TurnMode::Plan | TurnMode::PlanRevise | TurnMode::PlanFollowUp => {
                registry.restrict_to_planning();
            }
            TurnMode::PlanImplement | TurnMode::Normal | TurnMode::Init => {}
        }
        let specs = registry.specs();
        let system = self.system_prompt(&registry, mode == TurnMode::Goal, mode == TurnMode::Init);
        let overhead = context::prompt_tokens(&system, registry.tool_tokens());
        self.maybe_compact(overhead, sink, resolve_provider)?;

        let active_plan = self.session.active_plan();
        let implementation_started = matches!(
            active_plan,
            Some(crate::session::PlanState::Ready { revision, .. })
                if self.session.plan_has_handoff(*revision)
        );
        let note = plan_prompt(
            active_plan,
            plan_path.as_deref(),
            mode,
            implementation_started,
        )
        .map(crate::prompt::plan_phase_note);
        self.sync_plan_phase_note(note)?;

        if let Some(effort) = self.steers.take_reasoning_effort() {
            self.set_reasoning_effort(effort)?;
        }
        let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
        Ok(Step {
            registry,
            specs,
            system,
            overhead,
            provider,
            bare_model,
        })
    }

    /// Streams one response, compacting once on a context-limit error and
    /// asking again with a corrective note after a malformed tool call.
    /// `None` means the user interrupted.
    fn request_step<F>(
        &mut self,
        step: &Step,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<Option<TurnOutput>, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        let mut retried_context = false;
        // Request-only history for malformed tool-call retries; never
        // persisted, so the session keeps the clean transcript.
        let mut corrected: Option<Vec<Message>> = None;
        let mut tool_call_retries = 0;
        let out = loop {
            let request = provider::Request {
                model: &step.bare_model,
                system: &step.system,
                messages: corrected.as_deref().unwrap_or(self.session.messages()),
                tools: &step.specs,
                max_tokens: crate::model::max_tokens(&self.config, &self.model),
                supports_images: crate::model::supports_images(&self.config, &self.model),
                prompt_cache_control: true,
                prompt_cache_key: Some(self.prompt_cache_key()),
            };
            let response = stream_turn(step.provider.as_ref(), &request, &mut forward(sink));
            match response {
                Ok(out) => break out,
                Err(Error::Interrupted) => return Ok(None),
                Err(error)
                    if self.config.auto_compact
                        && !retried_context
                        && context::is_context_limit(&error) =>
                {
                    retried_context = true;
                    sink(TurnEvent::RetryReset);
                    sink(TurnEvent::Warning(
                        "Context limit reached; compacting before one retry".into(),
                    ));
                    self.compact_now_with(sink, resolve_provider)?;
                    corrected = None;
                }
                Err(Error::MalformedToolCall(message))
                    if tool_call_retries < MALFORMED_TOOL_CALL_RETRIES =>
                {
                    tool_call_retries += 1;
                    sink(TurnEvent::RetryReset);
                    sink(TurnEvent::Warning(format!(
                        "Model emitted a malformed tool call; asking it to retry \
                         ({tool_call_retries}/{MALFORMED_TOOL_CALL_RETRIES}): {message}"
                    )));
                    corrected.get_or_insert_with(|| {
                        let mut messages = self.session.messages().to_vec();
                        messages.push(Message::user(malformed_tool_call_note(&step.specs)));
                        messages
                    });
                }
                Err(error) => return Err(error),
            }
        };
        if crate::cancellation::interrupted() {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Records the response's usage and context and builds its assistant
    /// message, which the step handler persists.
    fn record_step(
        &mut self,
        out: TurnOutput,
        step: &Step,
        progress: &mut TurnProgress,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<(Message, Vec<ToolCall>), Error> {
        self.session.append_usage(out.usage)?;
        self.record_context(out.usage.total_tokens(), step.overhead)?;
        progress.budget.made = progress.budget.made.saturating_add(1);
        sink(TurnEvent::Usage {
            context_tokens: self.context_tokens,
            context_window: self.context_window(),
            request_usage: out.usage,
            session_usage: self.usage(),
        });
        self.latest_turn_result.clone_from(&out.text);
        if !out.tool_calls.is_empty() {
            progress.idle_plan_replies = 0;
        }
        let calls = out.tool_calls;
        let mut assistant = Message::assistant(out.text, calls.clone());
        assistant.reasoning = out.reasoning;
        assistant.provider_data = out.provider_data;
        if assistant.has_provider_replay_data() {
            assistant.provider_data_model = Some(step.bare_model.clone());
        }
        Ok((assistant, calls))
    }

    /// Executes tool calls in order. On interrupt, remaining calls get
    /// synthetic error results so every tool call keeps a paired result and
    /// the history stays valid for the next request.
    pub(super) fn run_tools(
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

    pub(super) fn run_tools_rejecting_goal_complete(
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
        for (index, call) in calls.iter().enumerate() {
            let result = if aborted {
                Message::tool_result(
                    &call.id,
                    &call.name,
                    "[interrupted by user]".to_string(),
                    true,
                )
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
                if let Some(path) = registry.mutating_path(&call.name, &call.arguments)
                    && let Some(Err(error)) = self.checkpoint_path(&path)
                {
                    sink(TurnEvent::Warning(format!(
                        "Could not record {} for /undo: {error}",
                        path.display()
                    )));
                }
                let outcome = registry.execute(
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
                }
                Message::tool_result_with_images(
                    &call.id,
                    &call.name,
                    outcome.content,
                    outcome.images,
                    outcome.is_error,
                )
            };
            if let Err(error) = self.save_tool_result(result) {
                self.pending_tool_results
                    .extend(calls[index + 1..].iter().map(|remaining| {
                        Message::tool_result(
                            &remaining.id,
                            &remaining.name,
                            "[not executed because an earlier tool result could not be saved]"
                                .into(),
                            true,
                        )
                    }));
                return Err(error);
            }
        }
        Ok(aborted)
    }

    /// Shows each new catalog warning once per conversation. Broken custom
    /// tool manifests and skills are otherwise visible only in `/tools`.
    fn report_catalog_warnings(
        &mut self,
        registry: &Registry,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) {
        if self.is_child() {
            return;
        }
        for warning in &registry.warnings {
            if self.reported_warnings.insert(warning.clone()) {
                sink(TurnEvent::Warning(format!("Skipped {warning}")));
            }
        }
    }

    pub(super) fn inject_pending_steers(
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

    fn drain_deferred_subagent_results_into_history(&mut self) -> Result<bool, Error> {
        let manager = self.services.subagents.clone();
        if !manager.has_deferred() {
            return Ok(false);
        }
        let deliveries = manager.drain_deferred();
        if deliveries.is_empty() {
            return Ok(false);
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
        Ok(true)
    }

    fn maybe_compact<F>(
        &mut self,
        overhead: u64,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        if !self.config.auto_compact
            || !compaction::should_compact(
                self.estimated_context(overhead),
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

    /// The system prompt for `registry`. `goal` includes the session's
    /// active goal and `init` adds the `/init` task; children use their own
    /// prompt.
    fn system_prompt(&self, registry: &Registry, goal: bool, init: bool) -> String {
        match &self.child {
            None => crate::prompt::build_system_prompt(
                &self.config.home_dir,
                registry.has_subagent_tools(),
                self.print_mode,
                registry.has_web_tools(),
                registry.skills(),
                crate::prompt::MainPromptState {
                    goal: goal.then(|| self.session.active_goal()).flatten(),
                    init,
                    custom_tools: registry.personalization().as_deref(),
                },
            ),
            Some(_) => crate::prompt::build_subagent_system_prompt(
                &self.config.home_dir,
                self.role_fragment(),
                registry.restriction_summary().as_deref(),
                registry.has_web_tools(),
                registry.skills(),
            ),
        }
    }

    /// Summarizes older history with the current model (`/compact`), inside
    /// this conversation's cancellation scope.
    pub(crate) fn compact(&mut self, sink: &mut dyn FnMut(TurnEvent<'_>)) -> Result<(), Error> {
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
        self.recover_history()?;
        sink(TurnEvent::Compacting);
        let registry = self.scan_tools();
        let specs = registry.specs();
        let system = self.system_prompt(&registry, true, false);
        if let Some(effort) = self.steers.take_reasoning_effort() {
            self.set_reasoning_effort(effort)?;
        }
        let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
        let (summary, range, summary_usage) = compaction::summarize(
            provider.as_ref(),
            &bare_model,
            crate::model::max_tokens(&self.config, &self.model),
            self.session.messages(),
            last_undoable_user_index(self.session.messages()),
            // Summarizer output is not user-facing; swallow its deltas.
            &mut |_| {},
        )?;
        self.session.append_usage(summary_usage)?;

        let remote_request = provider::Request {
            model: &bare_model,
            system: &system,
            messages: &self.session.messages()[range.clone()],
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
                    "Remote compaction failed; using the portable summary: {error}"
                )));
                None
            }
        };
        let mut request_usage = summary_usage;
        let (provider_data, provider_data_model) = if let Some(remote) = remote {
            self.session.append_usage(remote.usage)?;
            request_usage = request_usage.saturating_add(remote.usage);
            (remote.replacement_history, Some(bare_model.clone()))
        } else {
            (Vec::new(), None)
        };

        let replaced = range.len();
        self.session
            .append_compaction(&summary, range, provider_data, provider_data_model)?;
        // Old usage estimate is stale after compaction; a fresh number
        // arrives with the next response.
        self.context_tokens = 0;
        self.context_usage = None;
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

#[cfg(test)]
mod note_tests {
    use super::*;

    #[test]
    fn malformed_call_notes_name_the_available_tools() {
        let spec = |name: &str| {
            Arc::new(ToolSpec {
                name: name.into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            })
        };
        let note = malformed_tool_call_note(&[spec("read_file"), spec("list_files")]);
        assert!(note.starts_with(MALFORMED_TOOL_CALL_NOTE));
        assert!(note.ends_with("Available tools: read_file, list_files."));
        assert!(malformed_tool_call_note(&[]).ends_with("answer in plain text."));
    }
}
