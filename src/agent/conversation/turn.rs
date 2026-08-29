use crate::compaction;
use crate::config::Config;
use crate::error::Error;
use crate::provider::{self, Message, SubagentResult, ToolCall, TurnInput, stream_turn};
use crate::tools::Registry;

use super::Conversation;
use crate::agent::events::{TurnEvent, forward};

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
            self.run_turn_input_with(user_input, sink, &mut provider::resolve)
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
            let Some(timeout) = self.run_limits.and_then(|limits| limits.timeout) else {
                return self.run_turn_input_with(user_input, sink, &mut provider::resolve);
            };
            let (result, timed_out) =
                crate::cancellation::with_timeout(&cancellation, timeout, || {
                    self.run_turn_input_with(user_input, sink, &mut provider::resolve)
                });
            if timed_out {
                sink(TurnEvent::Warning("subagent timeout exceeded".into()));
            }
            result
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
            let Some(manager) = self.subagents.clone() else {
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
            .subagents
            .clone()
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
        self.latest_turn_result.clear();
        if let Some(input) = user_input {
            if !input.images.is_empty() && !crate::model::supports_images(&self.config, &self.model)
            {
                return Err(Error::Config(format!(
                    "model '{}' does not accept image input",
                    self.model
                )));
            }
            if let Some(checkpoints) = &mut self.checkpoints
                && let Err(error) = checkpoints.snapshot()
            {
                sink(TurnEvent::Warning(format!(
                    "Could not checkpoint for /undo: {error}"
                )));
            }
            self.append_input_message(Message::user_input(input))?;
        }
        // Per-run guard rails: only subagent conversations carry limits.
        let limits = self.run_limits;
        let max_requests = limits.map_or(0, |limits| limits.max_requests);
        let hard_requests = max_requests.saturating_add((max_requests / 2).max(1));
        let mut requests_made: u64 = 0;
        let mut steer_sent = false;

        // Uncapped: the loop ends when the model stops calling tools.
        loop {
            if crate::cancellation::interrupted() {
                return Ok(false);
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
            let registry = self.scan_tools();
            let specs = registry.specs();
            let system = if self.subagents.is_some() {
                crate::prompt::build_system_prompt(
                    &self.config.home_dir,
                    self.config.subagents,
                    self.print_mode,
                    registry.has_web_tools(),
                    registry.skills(),
                )
            } else {
                crate::prompt::build_subagent_system_prompt(
                    &self.config.home_dir,
                    self.role_fragment.as_deref(),
                    registry.has_web_tools(),
                    registry.skills(),
                )
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

            self.context_tokens = out.input_tokens.saturating_add(out.output_tokens);
            requests_made = requests_made.saturating_add(1);
            sink(TurnEvent::Usage {
                context_tokens: self.context_tokens,
                context_window: self.context_window(),
            });

            self.latest_turn_result.clone_from(&out.text);
            let mut assistant = Message::assistant(out.text, out.tool_calls.clone());
            assistant.reasoning = out.reasoning;
            assistant.provider_data = out.provider_data;
            self.session.append_message(&assistant)?;
            self.messages.push(assistant);
            sink(TurnEvent::AssistantDone);

            if out.tool_calls.is_empty() {
                return Ok(true);
            }
            let aborted = self.run_tools(&registry, &out.tool_calls, sink)?;
            if aborted {
                return Ok(false);
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
        self.run_tools_while(registry, calls, sink, crate::cancellation::interrupted)
    }

    pub(super) fn run_tools_while(
        &mut self,
        registry: &Registry,
        calls: &[ToolCall],
        sink: &mut dyn FnMut(TurnEvent<'_>),
        mut interrupted: impl FnMut() -> bool,
    ) -> Result<bool, Error> {
        let mut aborted = false;
        for call in calls {
            let result = if aborted || interrupted() {
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
                    && let Some(checkpoints) = &mut self.checkpoints
                    && let Err(error) = checkpoints.remember_path(&path)
                {
                    sink(TurnEvent::Warning(format!(
                        "Could not record {} for /undo: {error}",
                        path.display()
                    )));
                }
                let outcome = registry.execute_with_capabilities(
                    &call.name,
                    &call.arguments,
                    self.session.id(),
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
            self.session.append_message(&result)?;
            self.messages.push(result);
        }
        Ok(aborted)
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

    fn compact_now_with<F>(
        &mut self,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        sink(TurnEvent::Compacting);
        let (provider, bare_model) = resolve_provider(&self.model, &self.config)?;
        let (summary, replaced) = compaction::summarize(
            provider.as_ref(),
            &bare_model,
            crate::model::max_tokens(&self.config, &self.model),
            &self.messages,
            // Summarizer output is not user-facing; swallow its deltas.
            &mut |_| {},
        )?;
        self.session.append_compaction(&summary, replaced)?;
        compaction::apply_summary(&mut self.messages, &summary, replaced);
        // Old usage estimate is stale after compaction; a fresh number
        // arrives with the next response.
        self.context_tokens = 0;
        sink(TurnEvent::Compacted { replaced });
        Ok(())
    }
}
