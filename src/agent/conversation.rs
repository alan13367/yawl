//! Provider-neutral conversation state and turn execution.

mod context;
mod goal;
mod plan;
mod recovery;
mod steer;
mod step;
mod turn;

use std::time::Duration;

use crate::background::BackgroundProcessManager;
use crate::cancellation::CancellationToken;
use crate::checkpoint::Checkpoints;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{Message, MessageControl, TurnInput, UsageSummary};
use crate::session::{PlanState, Session};
use crate::subagent::SubagentManager;
use crate::tools::{CatalogCache, QuestionBroker, Registry};

pub(crate) use steer::SteerInbox;
pub(crate) use turn::TurnRequest;

/// Per-run guard rails for a memory-only subagent conversation. At
/// `max_requests` completed model requests a wrap-up instruction is injected;
/// at `max_requests + max_requests/2` (at least one extra) the run is stopped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunLimits {
    pub(crate) max_requests: u64,
    pub(crate) timeout: Option<Duration>,
}

/// Why a child run was stopped by its configured limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunStopReason {
    Timeout,
    RequestBudget,
}

impl RunStopReason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Timeout => "subagent timeout exceeded",
            Self::RequestBudget => "subagent request budget exceeded",
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

pub(crate) fn last_undoable_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(Message::starts_user_turn)
}

/// Session-bound workers and undo snapshots. Subagent conversations get
/// idle workers that never start anything and no snapshots.
struct Services {
    subagents: SubagentManager,
    background: BackgroundProcessManager,
    checkpoints: Option<Checkpoints>,
}

impl Services {
    /// Fresh workers and undo snapshots for the session `session_id`.
    fn persistent(config: &Config, session_id: &str, work_tree: std::path::PathBuf) -> Self {
        Self {
            subagents: SubagentManager::new(session_id.to_string(), config.max_subagents),
            background: BackgroundProcessManager::default(),
            checkpoints: Some(Checkpoints::open(&config.home_dir, session_id, work_tree)),
        }
    }

    fn child(session_id: &str) -> Self {
        Self {
            subagents: SubagentManager::new(session_id.to_string(), 1),
            background: BackgroundProcessManager::default(),
            checkpoints: None,
        }
    }

    fn shutdown(&self) {
        self.subagents.shutdown_and_discard();
        self.background.shutdown_and_discard();
    }
}

/// Identity and guard rails of a subagent conversation.
struct ChildPolicy {
    prompt_cache_key: String,
    run_limits: Option<RunLimits>,
    stop_reason: Option<RunStopReason>,
    tool_allowlist: Option<Vec<String>>,
    role_fragment: Option<String>,
}

/// Provider-neutral turn state. The main conversation records to a session
/// file; subagent conversations record to a memory-only session and carry a
/// [`ChildPolicy`].
pub(crate) struct Conversation {
    config: Config,
    /// Reasoning effort from the saved config, independent of a session choice.
    /// Reloading settings updates this. `/reasoning` does not.
    configured_reasoning_effort: Option<String>,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    model: String,
    /// The recorded history and everything derived from it.
    session: Session,
    services: Services,
    /// Set for subagent conversations.
    child: Option<ChildPolicy>,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    context_tokens: u64,
    context_usage: Option<crate::session::ContextUsage>,
    pending_tool_results: std::collections::VecDeque<Message>,
    latest_turn_result: String,
    /// Set when a turn ends through plan_complete so the TUI can offer the
    /// implement/revise handoff only for freshly finished plans.
    plan_ready_this_turn: bool,
    /// Mode of the last turn that stopped before the model finished, through
    /// an error or an interrupt, so `/continue` can resume it in place.
    unfinished_mode: Option<turn::TurnMode>,
    catalog_cache: CatalogCache,
    /// Catalog warnings already shown, so a broken manifest or skill is
    /// reported once instead of on every model step.
    reported_warnings: std::collections::HashSet<String>,
    cancellation: CancellationToken,
    print_mode: bool,
    steers: SteerInbox,
    questions: QuestionBroker,
}

impl Conversation {
    pub(super) fn persistent(
        config: Config,
        model: String,
        session: Session,
        work_tree: std::path::PathBuf,
    ) -> Self {
        let services = Services::persistent(&config, &session.id, work_tree);
        let mut conversation = Self::new(config, model, session, services, None);
        if let Some(effort) = conversation.session.recorded_reasoning() {
            conversation.config.reasoning_effort = effort;
        }
        conversation.adopt_recorded_context();
        conversation
    }

    pub(crate) fn memory(config: Config, model: String, session_id: String) -> Self {
        let session = Session::memory(session_id.clone(), &model);
        let services = Services::child(&session_id);
        let child = ChildPolicy {
            prompt_cache_key: session_id,
            run_limits: None,
            stop_reason: None,
            tool_allowlist: None,
            role_fragment: None,
        };
        Self::new(config, model, session, services, Some(child))
    }

    /// Turn state starts empty; the configured reasoning effort is kept
    /// apart so a session-chosen effort can be told from the default.
    fn new(
        config: Config,
        model: String,
        session: Session,
        services: Services,
        child: Option<ChildPolicy>,
    ) -> Self {
        Self {
            configured_reasoning_effort: config.reasoning_effort.clone(),
            config,
            model,
            session,
            services,
            child,
            context_tokens: 0,
            context_usage: None,
            pending_tool_results: Default::default(),
            latest_turn_result: String::new(),
            plan_ready_this_turn: false,
            unfinished_mode: None,
            catalog_cache: CatalogCache::default(),
            reported_warnings: Default::default(),
            cancellation: CancellationToken::default(),
            print_mode: false,
            steers: SteerInbox::default(),
            questions: QuestionBroker::default(),
        }
    }

    /// Adopts the session's last measured context when it still describes
    /// this model and history.
    fn adopt_recorded_context(&mut self) {
        self.context_usage = self
            .session
            .context()
            .filter(|usage| {
                usage.model == self.model && usage.messages <= self.session.messages().len()
            })
            .cloned();
        self.context_tokens = self.context_usage.as_ref().map_or(0, |usage| usage.tokens);
    }

    pub(crate) fn set_run_limits(&mut self, limits: RunLimits) {
        if let Some(child) = &mut self.child {
            child.run_limits = Some(limits);
        }
    }

    pub(crate) fn set_tool_allowlist(&mut self, tools: Vec<String>) {
        if let Some(child) = &mut self.child {
            child.tool_allowlist = Some(tools);
        }
    }

    pub(crate) fn set_role_fragment(&mut self, fragment: String) {
        if let Some(child) = &mut self.child {
            child.role_fragment = Some(fragment);
        }
    }

    pub(crate) fn set_prompt_cache_key(&mut self, key: String) {
        if let Some(child) = &mut self.child {
            child.prompt_cache_key = key;
        }
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
        self.session.messages()
    }

    /// A note about history the session log could not restore.
    pub(crate) fn session_warning(&self) -> Option<String> {
        self.session.replay_warning()
    }

    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn usage(&self) -> UsageSummary {
        self.session.usage()
    }

    fn prompt_cache_key(&self) -> &str {
        match &self.child {
            Some(child) => &child.prompt_cache_key,
            None => &self.session.id,
        }
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn steer_inbox(&self) -> SteerInbox {
        self.steers.clone()
    }

    pub(crate) fn question_broker(&self) -> QuestionBroker {
        self.questions.clone()
    }

    pub(crate) fn enable_interactive_questions(&self) {
        self.questions.enable();
    }

    pub(crate) fn active_goal(&self) -> Option<&str> {
        self.session.active_goal()
    }

    pub(crate) fn plan_state(&self) -> Option<&PlanState> {
        self.session.active_plan()
    }

    pub(crate) fn active_plan(&self) -> Option<&str> {
        self.plan_state().and_then(PlanState::ready)
    }

    pub(crate) fn plan_ready_this_turn(&self) -> bool {
        self.plan_ready_this_turn
    }

    pub(crate) fn take_unaccepted_steers(&self) -> Vec<TurnInput> {
        self.steers.drain()
    }

    /// Starts or replaces a persisted goal. The caller then runs a turn with
    /// no extra user input. Returns a checkpoint warning when snapshotting
    /// fails.
    pub(crate) fn start_goal(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        self.recover_history()?;
        let warning = self.checkpoint_warning();
        let message = Message::user_input(input).with_control(MessageControl::GoalStart);
        let goal = message.content.clone();
        self.session.append_goal_start(&goal, message)?;
        Ok(warning)
    }

    pub(crate) fn cancel_goal(&mut self) -> Result<bool, Error> {
        self.recover_history()?;
        if self.session.active_goal().is_none() {
            return Ok(false);
        }
        self.session.append_goal_cancel()?;
        Ok(true)
    }

    pub(crate) fn start_plan(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        self.recover_history()?;
        let objective = input.text.clone();
        if objective.trim().is_empty() {
            return Err(Error::Invalid("plan objective is empty".into()));
        }
        let warning = self.checkpoint_warning();
        let message = Message::user_input(input).with_control(MessageControl::PlanStart);
        self.session.append_plan_start(&objective, message)?;
        Ok(warning)
    }

    pub(crate) fn cancel_plan(&mut self) -> Result<bool, Error> {
        self.recover_history()?;
        if self.plan_state().is_none() {
            return Ok(false);
        }
        self.session.append_plan_cancel()?;
        Ok(true)
    }

    pub(super) fn clear_cancellation(&self) {
        self.cancellation.clear();
    }

    pub(super) fn set_print_mode(&mut self) {
        self.print_mode = true;
    }

    pub(crate) fn subagent_manager(&self) -> SubagentManager {
        self.services.subagents.clone()
    }

    pub(crate) fn background_manager(&self) -> BackgroundProcessManager {
        self.services.background.clone()
    }

    pub(crate) fn run_stop_reason(&self) -> Option<RunStopReason> {
        self.child.as_ref().and_then(|child| child.stop_reason)
    }

    pub(crate) fn latest_turn_result(&self) -> String {
        self.latest_turn_result.clone()
    }

    fn append_input_message(&mut self, message: Message) -> Result<(), Error> {
        self.session.append_message(message)
    }

    pub(crate) fn switch_model(&mut self, model: String) -> Result<(), Error> {
        self.adopt_model(model)
    }

    /// Uses `model` for later requests and records it in the session log, so
    /// resuming this session continues with the last used model.
    fn adopt_model(&mut self, model: String) -> Result<(), Error> {
        self.session.append_model_switch(&model)?;
        self.model = model;
        self.context_tokens = 0;
        self.context_usage = None;
        Ok(())
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) -> Result<(), Error> {
        if self.config.reasoning_effort == effort {
            return Ok(());
        }
        self.session.append_reasoning_effort(effort.clone())?;
        self.config.reasoning_effort = effort;
        Ok(())
    }

    pub(crate) fn sync_display_config(&mut self, config: &Config) {
        self.config.hide_reasoning = config.hide_reasoning;
        self.config.accent_color = config.accent_color;
        self.config.selection_color = config.selection_color;
        self.config.theme = config.theme;
        self.config.status_bar.clone_from(&config.status_bar);
        self.config.scroll_bar = config.scroll_bar;
        self.config.scroll_bar_auto_hide = config.scroll_bar_auto_hide;
        self.config.bell = config.bell;
    }

    /// Recovers what it can of the session being left. Whatever cannot be
    /// recovered stays in its log and is retried when it is resumed, so a
    /// damaged session never blocks starting or opening another one.
    fn leave_session(&mut self) {
        let _ = self.recover_history();
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        self.leave_session();
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        let old_id = self.session_id().to_string();
        let abandon_empty = !crate::session::has_message(&dirs.project, &old_id);
        let mut session = Session::create(&dirs.project, &cwd, &self.model)?;
        if self.config.reasoning_effort != self.configured_reasoning_effort {
            session.append_reasoning_effort(self.config.reasoning_effort.clone())?;
        }
        self.switch_session(session, cwd);
        self.context_tokens = 0;
        self.context_usage = None;
        self.latest_turn_result.clear();
        self.unfinished_mode = None;
        self.pending_tool_results.clear();
        let _ = self.steers.drain();
        Checkpoints::remove(&self.config.home_dir, &old_id);
        if abandon_empty {
            let _ = Session::delete(&dirs.project, &old_id);
        }
        Ok(())
    }

    /// Stops the current session's subagents and background processes, then
    /// makes `session` current.
    fn switch_session(&mut self, session: Session, work_tree: std::path::PathBuf) {
        self.services.shutdown();
        self.services = Services::persistent(&self.config, &session.id, work_tree);
        self.session = session;
    }

    /// Deletes a saved session. If it is the active session, starts a fresh one.
    pub fn delete_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        if self.session_id() == id {
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
        let id = self.session_id().to_string();
        if !self.messages().is_empty() || crate::session::has_message(&dirs.project, &id) {
            return Ok(false);
        }
        Session::delete(&dirs.project, &id)?;
        Checkpoints::remove(&self.config.home_dir, &id);
        Ok(true)
    }

    /// Replaces the conversation with a saved session (used by `/resume`).
    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let session = Session::open_searching(&self.config.session_dirs(&cwd).search, id)?;
        self.leave_session();
        // Continue with the model the session last used, unless its provider is
        // gone from the config.
        let model = session.resumed_model(&self.config, &self.model);
        self.switch_session(session, cwd);
        self.model = model;
        self.config.reasoning_effort = match self.session.recorded_reasoning() {
            Some(effort) => effort,
            None => self.configured_reasoning_effort.clone(),
        };
        self.adopt_recorded_context();
        self.latest_turn_result.clear();
        self.unfinished_mode = None;
        self.pending_tool_results.clear();
        let _ = self.steers.drain();
        self.recover_history()?;
        Ok(())
    }

    pub fn scan_tools(&mut self) -> Registry {
        let mut registry = match &self.child {
            Some(child) => Registry::scan_for_child(
                &self.config,
                &mut self.catalog_cache,
                child.tool_allowlist.as_deref(),
            ),
            None if self.config.subagents => Registry::scan_with_subagents_and_background(
                &self.config,
                &mut self.catalog_cache,
                self.services.subagents.clone(),
                &self.model,
                self.services.background.clone(),
            ),
            None => Registry::scan_with_background(
                &self.config,
                &mut self.catalog_cache,
                self.services.background.clone(),
            ),
        };
        if self.child.is_none() {
            if self.questions.is_enabled() {
                registry.advertise_user_input(self.questions.clone());
            }
            registry.advertise_define_tool(&self.config, &mut self.catalog_cache);
        }
        if self.session.active_plan().is_some() {
            registry.advertise_plan_tools();
        }
        registry
    }

    /// After a config reload, keep a session reasoning choice in effect.
    /// Saving a new default clears that choice so the session follows it.
    fn restore_session_reasoning(&mut self, reasoning_changed: bool) -> Result<(), Error> {
        self.configured_reasoning_effort
            .clone_from(&self.config.reasoning_effort);
        let recorded = if reasoning_changed {
            self.session.clear_reasoning_effort()?;
            None
        } else {
            self.session.recorded_reasoning()
        };
        if let Some(effort) = recorded {
            self.config.reasoning_effort = effort;
        }
        Ok(())
    }

    pub(crate) fn change_global_config(
        &mut self,
        change: ConfigChange,
    ) -> Result<ConfigChangeEffect, Error> {
        let kinds = ChangedSettings::of(std::slice::from_ref(&change));
        let outcome = self.config.change_global(change)?;
        self.adopt_changed_config(outcome.config, kinds)?;
        Ok(outcome.effect)
    }

    pub(crate) fn change_global_config_batch(
        &mut self,
        changes: Vec<ConfigChange>,
    ) -> Result<Vec<ConfigChangeEffect>, Error> {
        let kinds = ChangedSettings::of(&changes);
        let outcome = self.config.change_global_batch(changes)?;
        self.adopt_changed_config(outcome.config, kinds)?;
        Ok(outcome.effects)
    }

    /// Applies a saved config: new subagent capacity, the session's
    /// reasoning choice, and a changed default model.
    fn adopt_changed_config(
        &mut self,
        config: Config,
        changed: ChangedSettings,
    ) -> Result<(), Error> {
        self.config = config;
        self.services.subagents.set_limit(self.config.max_subagents);
        self.restore_session_reasoning(changed.reasoning)?;
        if changed.model {
            let model = self
                .config
                .model
                .clone()
                .ok_or_else(|| Error::Config("no model configured".into()))?;
            self.adopt_model(model)?;
        }
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
        if let Some(report) = self.recover_undo()? {
            return Ok(report);
        }
        self.recover_history()?;
        let messages = self.session.messages();
        let Some(start) = last_undoable_user_index(messages) else {
            return Ok(UndoReport::default());
        };
        let checkpoint = match &self.services.checkpoints {
            Some(checkpoints) => {
                checkpoints.ensure_loaded()?;
                checkpoints.last_index()
            }
            None => None,
        };
        let undo = crate::session::PendingUndo {
            dropped: messages.len() - start,
            clear_goal: messages[start..].iter().any(Message::is_goal_start),
            plan_state: self.session.plan_before_turn().cloned(),
            checkpoint,
            committed: false,
        };
        self.session.begin_undo(undo)?;
        Ok(self.recover_undo()?.unwrap_or_default())
    }

    /// Every path `write_file`/`edit_file` touched this session, each with its
    /// oldest pre-image. Child conversations track no checkpoints.
    pub fn touched_files(&self) -> Vec<crate::checkpoint::TouchedFile> {
        self.services
            .checkpoints
            .as_ref()
            .map(Checkpoints::touched_files)
            .unwrap_or_default()
    }

    fn is_child(&self) -> bool {
        self.child.is_some()
    }

    fn run_limits(&self) -> Option<RunLimits> {
        self.child.as_ref().and_then(|child| child.run_limits)
    }

    fn role_fragment(&self) -> Option<&str> {
        self.child
            .as_ref()
            .and_then(|child| child.role_fragment.as_deref())
    }

    fn set_stop_reason(&mut self, reason: Option<RunStopReason>) {
        if let Some(child) = &mut self.child {
            child.stop_reason = reason;
        }
    }

    fn checkpoint_snapshot(&mut self) -> Option<Result<(), Error>> {
        self.services
            .checkpoints
            .as_mut()
            .map(Checkpoints::snapshot)
    }

    /// Snapshots the work tree for `/undo`. A failure only becomes a warning
    /// because the turn can still run without it.
    fn checkpoint_warning(&mut self) -> Option<String> {
        self.checkpoint_snapshot()?
            .err()
            .map(|error| format!("Could not checkpoint for /undo: {error}"))
    }

    fn checkpoint_path(&mut self, path: &std::path::Path) -> Option<Result<(), Error>> {
        self.services
            .checkpoints
            .as_mut()
            .map(|checkpoints| checkpoints.remember_path(path))
    }
}

/// Which settings a config change touches that the conversation reacts to.
#[derive(Clone, Copy)]
struct ChangedSettings {
    model: bool,
    reasoning: bool,
}

impl ChangedSettings {
    fn of(changes: &[ConfigChange]) -> Self {
        Self {
            model: changes
                .iter()
                .any(|change| matches!(change, ConfigChange::Model(_))),
            reasoning: changes
                .iter()
                .any(|change| matches!(change, ConfigChange::ReasoningEffort { .. })),
        }
    }
}

#[cfg(test)]
mod tests;
