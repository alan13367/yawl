//! The agent loop: send messages → stream → execute tool calls → append
//! results → repeat until the model stops calling tools. Iterations are
//! uncapped; Ctrl+C aborts the in-flight turn, not the process. Memory-only
//! subagent conversations may carry [`RunLimits`] that steer a runaway run
//! toward completion and then stop it.

use std::time::Duration;

use crate::background::BackgroundProcessManager;
use crate::cancellation::CancellationToken;
use crate::checkpoint::{Checkpoints, RestoreReport};
use crate::compaction;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{
    self, Message, ReasoningKind, StreamNotice, SubagentResult, ToolCall, stream_turn,
};
use crate::session::Session;
use crate::subagent::SubagentManager;
use crate::tools::{DescribeCache, Registry};

/// Per-run guard rails for a memory-only subagent conversation. At
/// `max_requests` completed model requests a wrap-up instruction is injected;
/// at `max_requests + max_requests/2` (at least one extra) the run is stopped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunLimits {
    pub(crate) max_requests: u64,
    pub(crate) timeout: Option<Duration>,
}

/// Progress events surfaced to the UI (print mode or TUI) during a turn.
pub enum TurnEvent<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
    },
    /// Discard any partial text shown so far; a retry restarts the response.
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    /// One assistant response finished (there may be more after tools run).
    AssistantDone,
    /// A tool was selected but its arguments are still being generated.
    ToolPreparing {
        name: &'a str,
    },
    ToolStart {
        name: &'a str,
        args: &'a str,
    },
    ToolEnd {
        name: &'a str,
        output: &'a str,
        is_error: bool,
    },
    Compacting,
    Compacted {
        replaced: usize,
    },
    /// Non-fatal problem the user should know about; the turn continues.
    Warning(String),
    Usage {
        context_tokens: u64,
        context_window: u64,
    },
}

struct Journal {
    id: String,
    persistent: Option<Session>,
}

impl Journal {
    fn persistent(session: Session) -> Self {
        Self {
            id: session.id.clone(),
            persistent: Some(session),
        }
    }

    fn memory(id: String) -> Self {
        Self {
            id,
            persistent: None,
        }
    }

    fn append_message(&mut self, message: &Message) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_message(message),
            None => Ok(()),
        }
    }

    fn append_compaction(&mut self, summary: &str, replaced: usize) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_compaction(summary, replaced),
            None => Ok(()),
        }
    }

    fn append_undo(&mut self, dropped: usize) -> Result<(), Error> {
        match self.persistent.as_mut() {
            Some(session) => session.append_undo(dropped),
            None => Ok(()),
        }
    }
}

/// Result of `/undo`: how many messages were dropped and whether files/HEAD
/// were restored.
#[derive(Debug, Default)]
pub struct UndoReport {
    pub dropped: usize,
    pub restored_files: bool,
    pub reset_head: bool,
    pub warning: Option<String>,
}

pub(crate) fn is_undoable_user_prompt(message: &Message) -> bool {
    message.role == crate::provider::Role::User
        && message.subagent_results.is_empty()
        && !compaction::is_summary_message(message)
}

pub(crate) fn last_undoable_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(is_undoable_user_prompt)
}

/// Provider-neutral conversation state shared by the main agent and
/// memory-only subagents.
pub(crate) struct Conversation {
    config: Config,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    model: String,
    messages: Vec<Message>,
    session: Journal,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    context_tokens: u64,
    latest_turn_result: String,
    describe_cache: DescribeCache,
    cancellation: CancellationToken,
    subagents: Option<SubagentManager>,
    print_mode: bool,
    run_limits: Option<RunLimits>,
    /// Tool-name allowlist for preset children; `None` grants everything.
    tool_allowlist: Option<Vec<String>>,
    /// Extra instruction appended to the subagent role block by a preset.
    role_fragment: Option<String>,
    checkpoints: Option<Checkpoints>,
    background: Option<BackgroundProcessManager>,
}

impl Conversation {
    fn persistent(
        config: Config,
        model: String,
        session: Session,
        messages: Vec<Message>,
        work_tree: std::path::PathBuf,
    ) -> Self {
        let subagents = SubagentManager::new(session.id.clone(), config.max_subagents);
        let checkpoints = Some(Checkpoints::open(&config.home_dir, &session.id, work_tree));
        Self {
            config,
            model,
            messages,
            session: Journal::persistent(session),
            context_tokens: 0,
            latest_turn_result: String::new(),
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            subagents: Some(subagents),
            print_mode: false,
            run_limits: None,
            tool_allowlist: None,
            role_fragment: None,
            checkpoints,
            background: Some(BackgroundProcessManager::default()),
        }
    }

    pub(crate) fn memory(config: Config, model: String, session_id: String) -> Self {
        Self {
            config,
            model,
            messages: Vec::new(),
            session: Journal::memory(session_id),
            context_tokens: 0,
            latest_turn_result: String::new(),
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            subagents: None,
            print_mode: false,
            run_limits: None,
            tool_allowlist: None,
            role_fragment: None,
            checkpoints: None,
            background: None,
        }
    }

    pub(crate) fn set_run_limits(&mut self, limits: RunLimits) {
        self.run_limits = Some(limits);
    }

    pub(crate) fn set_tool_allowlist(&mut self, tools: Vec<String>) {
        self.tool_allowlist = Some(tools);
    }

    pub(crate) fn set_role_fragment(&mut self, fragment: String) {
        self.role_fragment = Some(fragment);
    }

    pub(crate) fn context_window(&self) -> u64 {
        crate::model::context_window(&self.config, &self.model)
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session.id
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn subagent_manager(&self) -> Option<SubagentManager> {
        self.subagents.clone()
    }

    pub(crate) fn background_manager(&self) -> Option<BackgroundProcessManager> {
        self.background.clone()
    }

    pub(crate) fn latest_turn_result(&self) -> String {
        self.latest_turn_result.clone()
    }

    fn append_input_message(&mut self, message: Message) -> Result<(), Error> {
        self.session.append_message(&message)?;
        self.messages.push(message);
        Ok(())
    }

    pub(crate) fn switch_model(&mut self, model: String) {
        self.model = model;
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.config.reasoning_effort = effort;
    }

    pub(crate) fn sync_display_config(&mut self, config: &Config) {
        self.config.hide_reasoning = config.hide_reasoning;
        self.config.accent_color = config.accent_color;
        self.config.scroll_bar = config.scroll_bar;
        self.config.scroll_bar_auto_hide = config.scroll_bar_auto_hide;
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        let old_id = self.session.id.clone();
        let abandon_empty = !crate::session::has_message(&dirs.project, &old_id);
        let session = Session::create(&dirs.project, &cwd, &self.model)?;
        if let Some(manager) = &self.subagents {
            manager.shutdown_and_discard();
        }
        if let Some(manager) = &self.background {
            manager.shutdown_and_discard();
        }
        self.subagents = Some(SubagentManager::new(
            session.id.clone(),
            self.config.max_subagents,
        ));
        self.background = Some(BackgroundProcessManager::default());
        self.session = Journal::persistent(session);
        self.messages.clear();
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        Checkpoints::remove(&self.config.home_dir, &old_id);
        if abandon_empty {
            let _ = Session::delete(&dirs.project, &old_id);
        }
        self.checkpoints = Some(Checkpoints::open(
            &self.config.home_dir,
            self.session.id.as_str(),
            cwd,
        ));
        Ok(())
    }

    /// Deletes a saved session. If it is the active session, starts a fresh one.
    pub fn delete_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        if self.session.id == id {
            let deleted_id = id.to_string();
            self.reset()?;
            // `reset` only removes an abandoned empty log; always unlink here.
            Session::delete(&dirs.project, &deleted_id)?;
            Checkpoints::remove(&self.config.home_dir, &deleted_id);
        } else {
            Session::delete(&dirs.project, id)?;
            Checkpoints::remove(&self.config.home_dir, id);
        }
        Ok(())
    }

    /// Removes the current session log when it never received a turn.
    pub fn discard_if_empty(&mut self) -> Result<bool, Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        let id = self.session.id.clone();
        if !self.messages.is_empty() || crate::session::has_message(&dirs.project, &id) {
            return Ok(false);
        }
        // Drop the open file handle before unlinking.
        self.session = Journal::memory(id.clone());
        Session::delete(&dirs.project, &id)?;
        Checkpoints::remove(&self.config.home_dir, &id);
        Ok(true)
    }

    /// Replaces the conversation with a saved session (used by `/resume`).
    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let (session, messages) =
            Session::open_searching(&self.config.session_dirs(&cwd).search, id)?;
        if let Some(manager) = &self.subagents {
            manager.shutdown_and_discard();
        }
        if let Some(manager) = &self.background {
            manager.shutdown_and_discard();
        }
        self.subagents = Some(SubagentManager::new(
            session.id.clone(),
            self.config.max_subagents,
        ));
        self.background = Some(BackgroundProcessManager::default());
        self.session = Journal::persistent(session);
        self.messages = messages;
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        self.checkpoints = Some(Checkpoints::open(
            &self.config.home_dir,
            self.session.id.as_str(),
            cwd,
        ));
        Ok(())
    }

    pub fn scan_tools(&mut self) -> Registry {
        let mut registry = match (&self.subagents, self.config.subagents, &self.background) {
            (Some(manager), true, Some(background)) => {
                Registry::scan_with_subagents_and_background(
                    &self.config,
                    &mut self.describe_cache,
                    manager.clone(),
                    &self.model,
                    background.clone(),
                )
            }
            (_, _, Some(background)) => Registry::scan_with_background(
                &self.config,
                &mut self.describe_cache,
                background.clone(),
            ),
            _ => Registry::scan(&self.config, &mut self.describe_cache),
        };
        if let Some(allowed) = &self.tool_allowlist {
            registry.retain_names(allowed);
        }
        registry
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        let changes_model = matches!(&change, ConfigChange::Model(_));
        let outcome = self.config.change_global(change)?;
        self.config = outcome.config;
        if let Some(manager) = &self.subagents {
            manager.set_limit(self.config.max_subagents);
        }
        if changes_model {
            self.model = self
                .config
                .model
                .clone()
                .ok_or_else(|| Error::Config("no model configured".into()))?;
            self.context_tokens = 0;
        }
        Ok(outcome.effect)
    }

    pub(crate) fn change_global_config_batch(
        &mut self,
        changes: Vec<ConfigChange>,
    ) -> Result<Vec<ConfigChangeEffect>, Error> {
        let changes_model = changes
            .iter()
            .any(|change| matches!(change, ConfigChange::Model(_)));
        let outcome = self.config.change_global_batch(changes)?;
        self.config = outcome.config;
        if let Some(manager) = &self.subagents {
            manager.set_limit(self.config.max_subagents);
        }
        if changes_model {
            self.model = self
                .config
                .model
                .clone()
                .ok_or_else(|| Error::Config("no model configured".into()))?;
            self.context_tokens = 0;
        }
        Ok(outcome.effects)
    }

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
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            self.cancellation.clear();
            self.run_turn_with(user_input, sink, &mut provider::resolve)
        })
    }

    pub(crate) fn run_turn_preserving_cancellation(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        let cancellation = self.cancellation.clone();
        crate::cancellation::scope(&cancellation, || {
            let Some(timeout) = self.run_limits.and_then(|limits| limits.timeout) else {
                return self.run_turn_with(user_input, sink, &mut provider::resolve);
            };
            let (result, timed_out) =
                crate::cancellation::with_timeout(&cancellation, timeout, || {
                    self.run_turn_with(user_input, sink, &mut provider::resolve)
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

    fn run_turn_with<F>(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
        resolve_provider: &mut F,
    ) -> Result<bool, Error>
    where
        F: FnMut(&str, &Config) -> Result<(Box<dyn provider::Provider>, String), Error>,
    {
        self.latest_turn_result.clear();
        if let Some(input) = user_input {
            if let Some(checkpoints) = &mut self.checkpoints
                && let Err(error) = checkpoints.snapshot()
            {
                sink(TurnEvent::Warning(format!(
                    "Could not checkpoint for /undo: {error}"
                )));
            }
            self.append_input_message(Message::user(input))?;
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
                    registry.skills(),
                )
            } else {
                crate::prompt::build_subagent_system_prompt(
                    &self.config.home_dir,
                    self.role_fragment.as_deref(),
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

    fn run_tools_while(
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
                let outcome = registry.execute(&call.name, &call.arguments, &self.session.id);
                sink(TurnEvent::ToolEnd {
                    name: &call.name,
                    output: &outcome.content,
                    is_error: outcome.is_error,
                });
                if interrupted() {
                    aborted = true;
                }
                Message::tool_result(&call.id, &call.name, outcome.content, outcome.is_error)
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

    /// Reverts the last user-initiated turn: restore files, then drop that
    /// prompt and every message after it.
    ///
    /// # Errors
    ///
    /// Returns session or checkpoint I/O errors. A missing turn is not an
    /// error; [`UndoReport::dropped`] is zero.
    pub fn undo_last_turn(&mut self) -> Result<UndoReport, Error> {
        let Some(start) = last_undoable_user_index(&self.messages) else {
            return Ok(UndoReport::default());
        };
        let dropped = self.messages.len() - start;
        let restore = match &mut self.checkpoints {
            Some(checkpoints) => checkpoints.restore_last()?,
            None => RestoreReport::default(),
        };
        self.session.append_undo(dropped)?;
        self.messages.truncate(start);
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        Ok(UndoReport {
            dropped,
            restored_files: restore.restored,
            reset_head: restore.reset_head,
            warning: restore.warning,
        })
    }
}

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

    pub(crate) fn subagents(&self) -> SubagentManager {
        self.conversation
            .subagent_manager()
            .expect("persistent agents always own a subagent manager")
    }

    pub(crate) fn background_processes(&self) -> BackgroundProcessManager {
        self.conversation
            .background_manager()
            .expect("persistent agents always own a background process manager")
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.conversation.cancellation_token()
    }

    pub(crate) fn clear_cancellation(&self) {
        self.conversation.cancellation.clear();
    }

    /// Disables automatic subagent follow-ups and adjusts orchestration
    /// guidance for a one-shot print-mode run.
    pub fn set_print_mode(&mut self) {
        self.conversation.print_mode = true;
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

    pub(crate) fn run_turn_preserving_cancellation(
        &mut self,
        user_input: Option<String>,
        sink: &mut dyn FnMut(TurnEvent<'_>),
    ) -> Result<bool, Error> {
        self.conversation
            .run_turn_preserving_cancellation(user_input, sink)
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

/// Adapts stream-level notices to turn events.
fn forward<'s>(sink: &'s mut dyn FnMut(TurnEvent<'_>)) -> impl FnMut(StreamNotice<'_>) + 's {
    move |notice| match notice {
        StreamNotice::TextDelta(t) => sink(TurnEvent::TextDelta(t)),
        StreamNotice::ReasoningDelta { kind, text } => {
            sink(TurnEvent::ReasoningDelta { kind, text })
        }
        StreamNotice::ToolPreparing { name } => sink(TurnEvent::ToolPreparing { name }),
        StreamNotice::RetryReset => sink(TurnEvent::RetryReset),
        StreamNotice::Retrying {
            attempt,
            delay_ms,
            error,
        } => sink(TurnEvent::Retrying {
            attempt,
            delay_ms,
            error,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::provider::{Event as ProviderEvent, Provider, Request, Role};

    enum ProviderStep {
        Output {
            text: &'static str,
            tool_calls: Vec<ToolCall>,
            input_tokens: u64,
            output_tokens: u64,
        },
        Fail,
    }

    struct ScriptedProvider {
        steps: Rc<RefCell<VecDeque<ProviderStep>>>,
        requests: Rc<RefCell<Vec<Vec<Role>>>>,
    }

    impl Provider for ScriptedProvider {
        fn stream_once(
            &self,
            request: &Request<'_>,
            on_event: &mut dyn FnMut(ProviderEvent),
        ) -> Result<(), Error> {
            self.requests.borrow_mut().push(
                request
                    .messages
                    .iter()
                    .map(|message| message.role)
                    .collect(),
            );
            let Some(step) = self.steps.borrow_mut().pop_front() else {
                return Err(Error::Protocol("test provider script exhausted".into()));
            };
            match step {
                ProviderStep::Output {
                    text,
                    tool_calls,
                    input_tokens,
                    output_tokens,
                } => {
                    if !text.is_empty() {
                        on_event(ProviderEvent::TextDelta(text.into()));
                    }
                    for call in tool_calls {
                        on_event(ProviderEvent::ToolCall(call));
                    }
                    on_event(ProviderEvent::Usage {
                        input_tokens,
                        output_tokens,
                    });
                    on_event(ProviderEvent::Done);
                    Ok(())
                }
                ProviderStep::Fail => Err(Error::Protocol("scripted failure".into())),
            }
        }
    }

    struct TestAgent {
        root: PathBuf,
        sessions_dir: PathBuf,
        agent: Conversation,
    }

    impl TestAgent {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("yawl-agent-{}-{nonce}-{name}", std::process::id()));
            let home_dir = root.join("home");
            let project_dir = root.join("project");
            let config = Config {
                model: Some("test".into()),
                auto_compact: false,
                home_dir: home_dir.clone(),
                project_dir,
                ..Config::test_default()
            };
            let cwd = root.join("cwd");
            let _ = std::fs::create_dir_all(&cwd);
            let dirs = config.session_dirs(&cwd);
            let session = Session::create(&dirs.project, &cwd, "test")
                .expect("test session should be created");
            Self {
                root,
                sessions_dir: dirs.project,
                agent: Conversation::persistent(config, "test".into(), session, Vec::new(), cwd),
            }
        }
    }

    impl Drop for TestAgent {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn tool_allowlist_filters_child_tool_scans() {
        let test = TestAgent::new("allowlist");
        let mut child =
            Conversation::memory(test.agent.config().clone(), "test".into(), "child".into());
        child.set_tool_allowlist(vec!["read_file".into()]);

        let mut names = child
            .scan_tools()
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["read_file"],
            "preset children see only allowed tools"
        );
    }

    #[test]
    fn print_mode_pump_delivers_deferred_results_in_a_follow_up_turn() {
        let mut test = TestAgent::new("subagent-pump");
        test.agent.config.subagents = true;
        test.agent
            .subagents
            .as_ref()
            .expect("persistent conversations own a manager")
            .push_test_deferred(
                1,
                "scout",
                crate::subagent::RunOutcome::Completed,
                "found it",
            );
        let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
            text: "delivered summary",
            tool_calls: Vec::new(),
            input_tokens: 10,
            output_tokens: 2,
        }])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        // Drive the pump's delivery turn with the injected resolver the same
        // way run drives it with the real one.
        let pumped = test
            .agent
            .pump_subagent_results_with(&mut |_| {}, 1, &mut resolve)
            .expect("pump should complete");

        assert!(pumped);
        assert!(
            !test
                .agent
                .subagents
                .as_ref()
                .expect("manager survives the pump")
                .has_deferred()
        );
        let messages = &test.agent.messages;
        assert_eq!(
            messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [Role::User, Role::Assistant]
        );
        assert!(
            messages[0].content.contains("Background subagent results"),
            "the delivery message should carry the results; got:\n{}",
            messages[0].content
        );
        assert!(
            messages[0].content.contains("found it"),
            "the delivery message should carry the subagent result text"
        );
        assert_eq!(messages[1].content, "delivered summary");
        assert_eq!(requests.borrow().len(), 1);
    }

    #[test]
    fn print_mode_pump_with_no_active_subagents_returns_promptly() {
        let mut test = TestAgent::new("subagent-pump-idle");
        test.agent.print_mode = true;
        let pumped = test
            .agent
            .pump_subagent_results_with(&mut |_| {}, 1, &mut |_, _| unreachable!())
            .expect("idle pump should complete immediately");
        assert!(pumped);
    }

    #[test]
    fn conversation_transaction_persists_tool_loop_in_order() {
        let mut test = TestAgent::new("tool-loop");
        let steps = Rc::new(RefCell::new(VecDeque::from([
            ProviderStep::Output {
                text: "",
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: r#"{"command":"printf tool-output"}"#.into(),
                }],
                input_tokens: 10,
                output_tokens: 2,
            },
            ProviderStep::Output {
                text: "done",
                tool_calls: Vec::new(),
                input_tokens: 10,
                output_tokens: 2,
            },
        ])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        let completed = test
            .agent
            .run_turn_with(Some("run it".into()), &mut |_| {}, &mut resolve)
            .expect("scripted turn should complete");

        assert!(completed);
        assert_eq!(
            test.agent
                .messages
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
        assert_eq!(test.agent.messages[2].content, "tool-output");
        assert_eq!(test.agent.messages[3].content, "done");
        assert_eq!(
            requests.borrow().as_slice(),
            [
                vec![Role::User],
                vec![Role::User, Role::Assistant, Role::Tool]
            ]
        );
        let (_, replayed) = Session::open(&test.sessions_dir, &test.agent.session.id)
            .expect("persisted session should replay");
        assert_eq!(
            replayed
                .iter()
                .map(|message| message.role)
                .collect::<Vec<_>>(),
            [Role::User, Role::Assistant, Role::Tool, Role::Assistant]
        );
    }

    #[test]
    fn failed_provider_does_not_persist_partial_assistant() {
        let mut test = TestAgent::new("provider-failure");
        let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Fail])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        let result = test
            .agent
            .run_turn_with(Some("hello".into()), &mut |_| {}, &mut resolve);

        assert!(result.is_err());
        assert_eq!(test.agent.messages.len(), 1);
        assert_eq!(test.agent.messages[0].role, Role::User);
    }

    #[test]
    fn provider_usage_saturates_instead_of_wrapping() {
        let mut test = TestAgent::new("saturating-usage");
        let steps = Rc::new(RefCell::new(VecDeque::from([ProviderStep::Output {
            text: "done",
            tool_calls: Vec::new(),
            input_tokens: u64::MAX,
            output_tokens: 1,
        }])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };

        let completed = test
            .agent
            .run_turn_with(Some("hello".into()), &mut |_| {}, &mut resolve)
            .expect("scripted turn should complete");

        assert!(completed);
        assert_eq!(test.agent.context_tokens(), u64::MAX);
    }

    #[test]
    fn failed_auto_compaction_warns_and_the_turn_continues() {
        let mut test = TestAgent::new("compact-warning");
        test.agent.config.auto_compact = true;
        test.agent.config.context_windows.insert("test".into(), 10);
        // Enough history that auto-compaction has something to summarize.
        for index in 0..12 {
            test.agent
                .messages
                .push(Message::user(format!("history {index}")));
        }
        let steps = Rc::new(RefCell::new(VecDeque::from([
            ProviderStep::Output {
                text: "",
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "shell".into(),
                    arguments: r#"{"command":"true"}"#.into(),
                }],
                input_tokens: 100,
                output_tokens: 0,
            },
            ProviderStep::Fail,
            ProviderStep::Output {
                text: "done",
                tool_calls: Vec::new(),
                input_tokens: 100,
                output_tokens: 0,
            },
        ])));
        let requests = Rc::new(RefCell::new(Vec::new()));
        let mut resolve = |_: &str, _: &Config| {
            Ok::<(Box<dyn Provider>, String), Error>((
                Box::new(ScriptedProvider {
                    steps: Rc::clone(&steps),
                    requests: Rc::clone(&requests),
                }),
                "test".into(),
            ))
        };
        let mut warnings = Vec::new();

        let completed = test
            .agent
            .run_turn_with(
                Some("hello".into()),
                &mut |event| {
                    if let TurnEvent::Warning(text) = event {
                        warnings.push(text.to_string());
                    }
                },
                &mut resolve,
            )
            .expect("turn should survive the compaction failure");

        assert!(completed);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Auto-compaction failed"));
        // The failed compaction left the conversation untouched.
        assert_eq!(test.agent.messages.len(), 16);
        assert_eq!(test.agent.messages[14].role, Role::Tool);
        assert_eq!(test.agent.messages[15].content, "done");
    }

    #[test]
    fn interrupted_tool_batch_keeps_one_result_per_call() {
        let mut test = TestAgent::new("interrupted-tools");
        let registry = test.agent.scan_tools();
        let calls = [
            ToolCall {
                id: "one".into(),
                name: "shell".into(),
                arguments: r#"{"command":"true"}"#.into(),
            },
            ToolCall {
                id: "two".into(),
                name: "shell".into(),
                arguments: r#"{"command":"true"}"#.into(),
            },
        ];

        let aborted = test
            .agent
            .run_tools_while(&registry, &calls, &mut |_| {}, || true)
            .expect("synthetic tool results should persist");

        assert!(aborted);
        assert_eq!(test.agent.messages.len(), 2);
        assert!(test.agent.messages.iter().all(|message| {
            message.role == Role::Tool
                && message.is_error
                && message.content == "[interrupted by user]"
        }));
        assert_eq!(test.agent.messages[0].tool_call_id.as_deref(), Some("one"));
        assert_eq!(test.agent.messages[1].tool_call_id.as_deref(), Some("two"));
    }

    #[test]
    fn last_undoable_user_skips_summaries_and_subagent_results() {
        let summary = compaction::summary_message("old");
        let subagent = Message::subagent_results(vec![crate::provider::SubagentResult {
            id: "sa-1".into(),
            name: "scout".into(),
            status: "completed".into(),
            run_number: 1,
            content: "ok".into(),
        }]);
        let messages = [
            Message::user("keep"),
            Message::assistant("a".into(), vec![]),
            summary,
            Message::assistant("b".into(), vec![]),
            Message::user("undo-me"),
            Message::assistant("c".into(), vec![]),
            subagent,
        ];
        assert_eq!(last_undoable_user_index(&messages), Some(4));
        assert!(is_undoable_user_prompt(&messages[0]));
        assert!(!is_undoable_user_prompt(&messages[2]));
        assert!(!is_undoable_user_prompt(&messages[6]));
    }

    #[test]
    fn undo_last_turn_drops_the_prompt_and_restores_files() {
        let mut test = TestAgent::new("undo-turn");
        let file = test.root.join("cwd").join("note.txt");
        std::fs::write(&file, "before").expect("write");
        test.agent
            .checkpoints
            .as_mut()
            .expect("persistent")
            .snapshot()
            .expect("snapshot");
        test.agent
            .checkpoints
            .as_mut()
            .expect("persistent")
            .remember_path(&file)
            .expect("pre-image");
        test.agent
            .append_input_message(Message::user("edit the file"))
            .expect("user");
        test.agent
            .append_input_message(Message::assistant("done".into(), vec![]))
            .expect("assistant");
        std::fs::write(&file, "after").expect("mutate");

        let report = test.agent.undo_last_turn().expect("undo");
        assert_eq!(report.dropped, 2);
        assert!(test.agent.messages.is_empty());
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "before");
    }

    #[test]
    fn undo_without_a_user_turn_is_a_no_op() {
        let mut test = TestAgent::new("undo-empty");
        let report = test.agent.undo_last_turn().expect("undo");
        assert_eq!(report.dropped, 0);
        assert!(test.agent.messages.is_empty());
    }
}
