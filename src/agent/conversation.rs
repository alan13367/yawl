//! Provider-neutral conversation state and turn execution.

mod goal;
mod plan;
mod steer;
mod turn;

use std::time::Duration;

use crate::background::BackgroundProcessManager;
use crate::cancellation::CancellationToken;
use crate::checkpoint::Checkpoints;
use crate::compaction;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{Message, MessageControl, TokenUsage, TurnInput, UsageSummary};
use crate::session::{PlanState, Session};
use crate::subagent::SubagentManager;
use crate::tools::{DescribeCache, QuestionBroker, Registry};

pub(crate) use steer::SteerInbox;

/// Per-run guard rails for a memory-only subagent conversation. At
/// `max_requests` completed model requests a wrap-up instruction is injected;
/// at `max_requests + max_requests/2` (at least one extra) the run is stopped.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RunLimits {
    pub(crate) max_requests: u64,
    pub(crate) timeout: Option<Duration>,
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
        && (!message.is_hidden_control() || message.is_plan_implementation_start())
        && !message.is_steering()
}

pub(crate) fn last_undoable_user_index(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(is_undoable_user_prompt)
}

struct PersistentState {
    session: Session,
    subagents: SubagentManager,
    checkpoints: Checkpoints,
    background: BackgroundProcessManager,
    active_goal: Option<String>,
}

struct ChildState {
    session_id: String,
    prompt_cache_key: String,
    usage: UsageSummary,
    run_limits: Option<RunLimits>,
    tool_allowlist: Option<Vec<String>>,
    role_fragment: Option<String>,
}

enum ConversationKind {
    Persistent(PersistentState),
    Child(ChildState),
}

/// Provider-neutral turn state with explicit persistent-agent and
/// memory-only-child capabilities.
pub(crate) struct Conversation {
    config: Config,
    /// Current model spec (may carry an `anthropic:`/`openai:` prefix);
    /// switchable mid-session via `/model`.
    model: String,
    messages: Vec<Message>,
    kind: ConversationKind,
    /// Last provider-reported total (input + output) tokens — the best
    /// estimate of current context usage.
    context_tokens: u64,
    latest_turn_result: String,
    /// Set when a turn ends through plan_complete so the TUI can offer the
    /// implement/revise handoff only for freshly finished plans.
    plan_ready_this_turn: bool,
    describe_cache: DescribeCache,
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
        messages: Vec<Message>,
        work_tree: std::path::PathBuf,
    ) -> Self {
        let subagents = SubagentManager::new(session.id.clone(), config.max_subagents);
        let checkpoints = Checkpoints::open(&config.home_dir, &session.id, work_tree);
        let active_goal = session.active_goal().map(str::to_string);
        Self {
            config,
            model,
            messages,
            kind: ConversationKind::Persistent(PersistentState {
                session,
                subagents,
                checkpoints,
                background: BackgroundProcessManager::default(),
                active_goal,
            }),
            context_tokens: 0,
            latest_turn_result: String::new(),
            plan_ready_this_turn: false,
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            print_mode: false,
            steers: SteerInbox::default(),
            questions: QuestionBroker::default(),
        }
    }

    pub(crate) fn memory(config: Config, model: String, session_id: String) -> Self {
        Self {
            config,
            model,
            messages: Vec::new(),
            kind: ConversationKind::Child(ChildState {
                prompt_cache_key: session_id.clone(),
                session_id,
                usage: UsageSummary::default(),
                run_limits: None,
                tool_allowlist: None,
                role_fragment: None,
            }),
            context_tokens: 0,
            latest_turn_result: String::new(),
            plan_ready_this_turn: false,
            describe_cache: DescribeCache::default(),
            cancellation: CancellationToken::default(),
            print_mode: false,
            steers: SteerInbox::default(),
            questions: QuestionBroker::default(),
        }
    }

    pub(crate) fn set_run_limits(&mut self, limits: RunLimits) {
        self.child_mut().run_limits = Some(limits);
    }

    pub(crate) fn set_tool_allowlist(&mut self, tools: Vec<String>) {
        self.child_mut().tool_allowlist = Some(tools);
    }

    pub(crate) fn set_role_fragment(&mut self, fragment: String) {
        self.child_mut().role_fragment = Some(fragment);
    }

    pub(crate) fn set_prompt_cache_key(&mut self, key: String) {
        self.child_mut().prompt_cache_key = key;
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
        match &self.kind {
            ConversationKind::Persistent(state) => &state.session.id,
            ConversationKind::Child(state) => &state.session_id,
        }
    }

    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn usage(&self) -> UsageSummary {
        match &self.kind {
            ConversationKind::Persistent(state) => state.session.usage(),
            ConversationKind::Child(state) => state.usage,
        }
    }

    fn prompt_cache_key(&self) -> &str {
        match &self.kind {
            ConversationKind::Persistent(state) => &state.session.id,
            ConversationKind::Child(state) => &state.prompt_cache_key,
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
        self.persistent_state().active_goal.as_deref()
    }

    pub(crate) fn plan_state(&self) -> Option<&PlanState> {
        self.persistent_state().session.active_plan()
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
        let warning = self
            .persistent_mut()
            .checkpoints
            .snapshot()
            .err()
            .map(|error| format!("Could not checkpoint for /undo: {error}"));
        let message = Message::user_input(input).with_control(MessageControl::GoalStart);
        let goal = message.content.clone();
        self.persistent_mut()
            .session
            .append_goal_start(&goal, &message)?;
        self.messages.push(message);
        self.persistent_mut().active_goal = Some(goal);
        Ok(warning)
    }

    pub(crate) fn cancel_goal(&mut self) -> Result<bool, Error> {
        if self.persistent_state().active_goal.is_none() {
            return Ok(false);
        }
        let state = self.persistent_mut();
        state.session.append_goal_cancel()?;
        state.active_goal = None;
        Ok(true)
    }

    pub(crate) fn start_plan(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        let objective = input.text.clone();
        if objective.trim().is_empty() {
            return Err(Error::Config("plan objective is empty".into()));
        }
        let warning = self
            .persistent_mut()
            .checkpoints
            .snapshot()
            .err()
            .map(|error| format!("Could not checkpoint for /undo: {error}"));
        let message = Message::user_input(input);
        self.persistent_mut()
            .session
            .append_plan_start(&objective, &message)?;
        self.messages.push(message);
        Ok(warning)
    }

    pub(crate) fn cancel_plan(&mut self) -> Result<bool, Error> {
        if self.plan_state().is_none() {
            return Ok(false);
        }
        self.persistent_mut().session.append_plan_cancel()?;
        Ok(true)
    }

    pub(super) fn clear_cancellation(&self) {
        self.cancellation.clear();
    }

    pub(super) fn set_print_mode(&mut self) {
        self.print_mode = true;
    }

    pub(crate) fn subagent_manager(&self) -> SubagentManager {
        self.persistent_state().subagents.clone()
    }

    pub(crate) fn background_manager(&self) -> BackgroundProcessManager {
        self.persistent_state().background.clone()
    }

    pub(crate) fn latest_turn_result(&self) -> String {
        self.latest_turn_result.clone()
    }

    fn append_input_message(&mut self, message: Message) -> Result<(), Error> {
        self.persist_message(&message)?;
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
        self.config.status_bar.clone_from(&config.status_bar);
        self.config.scroll_bar = config.scroll_bar;
        self.config.scroll_bar_auto_hide = config.scroll_bar_auto_hide;
        self.config.bell = config.bell;
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        let old_id = self.session_id().to_string();
        let abandon_empty = !crate::session::has_message(&dirs.project, &old_id);
        let session = Session::create(&dirs.project, &cwd, &self.model)?;
        self.persistent_state().subagents.shutdown_and_discard();
        self.persistent_state().background.shutdown_and_discard();
        let session_id = session.id.clone();
        self.kind = ConversationKind::Persistent(PersistentState {
            session,
            subagents: SubagentManager::new(session_id.clone(), self.config.max_subagents),
            checkpoints: Checkpoints::open(&self.config.home_dir, &session_id, cwd),
            background: BackgroundProcessManager::default(),
            active_goal: None,
        });
        self.messages.clear();
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        let _ = self.steers.drain();
        Checkpoints::remove(&self.config.home_dir, &old_id);
        if abandon_empty {
            let _ = Session::delete(&dirs.project, &old_id);
        }
        Ok(())
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
        if !self.messages.is_empty() || crate::session::has_message(&dirs.project, &id) {
            return Ok(false);
        }
        Session::delete(&dirs.project, &id)?;
        Checkpoints::remove(&self.config.home_dir, &id);
        Ok(true)
    }

    /// Replaces the conversation with a saved session (used by `/resume`).
    pub fn load_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let (session, messages) =
            Session::open_searching(&self.config.session_dirs(&cwd).search, id)?;
        let active_goal = session.active_goal().map(str::to_string);
        self.persistent_state().subagents.shutdown_and_discard();
        self.persistent_state().background.shutdown_and_discard();
        let session_id = session.id.clone();
        self.kind = ConversationKind::Persistent(PersistentState {
            session,
            subagents: SubagentManager::new(session_id.clone(), self.config.max_subagents),
            checkpoints: Checkpoints::open(&self.config.home_dir, &session_id, cwd),
            background: BackgroundProcessManager::default(),
            active_goal,
        });
        self.messages = messages;
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        let _ = self.steers.drain();
        Ok(())
    }

    pub fn scan_tools(&mut self) -> Registry {
        let mut registry = match &self.kind {
            ConversationKind::Persistent(state) if self.config.subagents => {
                Registry::scan_with_subagents_and_background(
                    &self.config,
                    &mut self.describe_cache,
                    state.subagents.clone(),
                    &self.model,
                    state.background.clone(),
                )
            }
            ConversationKind::Persistent(state) => Registry::scan_with_background(
                &self.config,
                &mut self.describe_cache,
                state.background.clone(),
            ),
            ConversationKind::Child(_) => Registry::scan(&self.config, &mut self.describe_cache),
        };
        if let ConversationKind::Child(ChildState {
            tool_allowlist: Some(allowed),
            ..
        }) = &self.kind
        {
            registry.retain_names(allowed);
        }
        if matches!(&self.kind, ConversationKind::Persistent(_)) && self.questions.is_enabled() {
            registry.advertise_user_input(self.questions.clone());
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
        if let ConversationKind::Persistent(state) = &self.kind {
            state.subagents.set_limit(self.config.max_subagents);
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
        if let ConversationKind::Persistent(state) = &self.kind {
            state.subagents.set_limit(self.config.max_subagents);
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
        let clear_goal = self.messages[start..].iter().any(Message::is_goal_start);
        let plan_state = self.persistent_state().session.plan_before_turn().cloned();
        let restore = self.persistent_mut().checkpoints.restore_last()?;
        self.persistent_mut()
            .session
            .append_undo_event_with_plan(dropped, clear_goal, plan_state)?;
        self.messages.truncate(start);
        if clear_goal {
            self.persistent_mut().active_goal = None;
        }
        self.context_tokens = 0;
        self.latest_turn_result.clear();
        Ok(UndoReport {
            dropped,
            restored_files: restore.restored,
            reset_head: restore.reset_head,
            warning: restore.warning,
        })
    }

    fn persistent_state(&self) -> &PersistentState {
        match &self.kind {
            ConversationKind::Persistent(state) => state,
            ConversationKind::Child(_) => panic!("operation requires a persistent conversation"),
        }
    }

    /// Every path `write_file`/`edit_file` touched this session, each with its
    /// oldest pre-image. Child conversations track no checkpoints.
    pub fn touched_files(&self) -> Vec<crate::checkpoint::TouchedFile> {
        match &self.kind {
            ConversationKind::Persistent(state) => state.checkpoints.touched_files(),
            ConversationKind::Child(_) => Vec::new(),
        }
    }

    fn persistent_mut(&mut self) -> &mut PersistentState {
        match &mut self.kind {
            ConversationKind::Persistent(state) => state,
            ConversationKind::Child(_) => panic!("operation requires a persistent conversation"),
        }
    }

    fn child_mut(&mut self) -> &mut ChildState {
        match &mut self.kind {
            ConversationKind::Child(state) => state,
            ConversationKind::Persistent(_) => panic!("operation requires a child conversation"),
        }
    }

    fn run_limits(&self) -> Option<RunLimits> {
        match &self.kind {
            ConversationKind::Child(state) => state.run_limits,
            ConversationKind::Persistent(_) => None,
        }
    }

    fn role_fragment(&self) -> Option<&str> {
        match &self.kind {
            ConversationKind::Child(state) => state.role_fragment.as_deref(),
            ConversationKind::Persistent(_) => None,
        }
    }

    fn persistent_subagents(&self) -> Option<SubagentManager> {
        match &self.kind {
            ConversationKind::Persistent(state) => Some(state.subagents.clone()),
            ConversationKind::Child(_) => None,
        }
    }

    fn checkpoint_snapshot(&mut self) -> Option<Result<(), Error>> {
        match &mut self.kind {
            ConversationKind::Persistent(state) => Some(state.checkpoints.snapshot()),
            ConversationKind::Child(_) => None,
        }
    }

    fn checkpoint_path(&mut self, path: &std::path::Path) -> Option<Result<(), Error>> {
        match &mut self.kind {
            ConversationKind::Persistent(state) => Some(state.checkpoints.remember_path(path)),
            ConversationKind::Child(_) => None,
        }
    }

    fn persist_message(&mut self, message: &Message) -> Result<(), Error> {
        match &mut self.kind {
            ConversationKind::Persistent(state) => state.session.append_message(message),
            ConversationKind::Child(_) => Ok(()),
        }
    }

    fn persist_compaction(
        &mut self,
        summary: &str,
        start: usize,
        replaced: usize,
        provider_data: &[serde_json::Value],
        provider_data_model: Option<&str>,
    ) -> Result<(), Error> {
        match &mut self.kind {
            ConversationKind::Persistent(state) => {
                state.session.append_compaction_range_with_provider_data(
                    summary,
                    start,
                    replaced,
                    provider_data,
                    provider_data_model,
                )
            }
            ConversationKind::Child(_) => Ok(()),
        }
    }

    fn record_usage(&mut self, usage: TokenUsage) -> Result<(), Error> {
        match &mut self.kind {
            ConversationKind::Persistent(state) => state.session.append_usage(usage),
            ConversationKind::Child(state) => {
                state.usage.record(usage);
                Ok(())
            }
        }
    }

    fn record_cache_reset(&mut self) -> Result<(), Error> {
        match &mut self.kind {
            // Persistent compactions record the reset in their existing
            // append-only compaction event.
            ConversationKind::Persistent(_) => Ok(()),
            ConversationKind::Child(state) => {
                state.usage.record_cache_reset();
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;
