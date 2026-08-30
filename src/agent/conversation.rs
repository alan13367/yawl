//! Provider-neutral conversation state and turn execution.

mod goal;
mod steer;
mod turn;

use std::time::Duration;

use crate::background::BackgroundProcessManager;
use crate::cancellation::CancellationToken;
use crate::checkpoint::{Checkpoints, RestoreReport};
use crate::compaction;
use crate::config::{Config, ConfigChange, ConfigChangeEffect};
use crate::error::Error;
use crate::provider::{Message, MessageControl, TurnInput};
use crate::session::Session;
use crate::subagent::SubagentManager;
use crate::tools::{DescribeCache, Registry};

use super::journal::Journal;

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
        && !message.is_hidden_control()
        && !message.is_steering()
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
    active_goal: Option<String>,
    steers: SteerInbox,
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
        let checkpoints = Some(Checkpoints::open(&config.home_dir, &session.id, work_tree));
        let active_goal = session.active_goal().map(str::to_string);
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
            active_goal,
            steers: SteerInbox::default(),
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
            active_goal: None,
            steers: SteerInbox::default(),
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
        self.session.id()
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

    pub(crate) fn steer_inbox(&self) -> SteerInbox {
        self.steers.clone()
    }

    pub(crate) fn active_goal(&self) -> Option<&str> {
        self.active_goal.as_deref()
    }

    pub(crate) fn take_unaccepted_steers(&self) -> Vec<TurnInput> {
        self.steers.drain()
    }

    /// Starts or replaces a persisted goal. The caller then runs a turn with
    /// no extra user input. Returns a checkpoint warning when snapshotting
    /// fails.
    pub(crate) fn start_goal(&mut self, input: TurnInput) -> Result<Option<String>, Error> {
        let warning = if let Some(checkpoints) = &mut self.checkpoints {
            checkpoints
                .snapshot()
                .err()
                .map(|error| format!("Could not checkpoint for /undo: {error}"))
        } else {
            None
        };
        let message = Message::user_input(input).with_control(MessageControl::GoalStart);
        let goal = message.content.clone();
        self.session.append_goal_start(&goal, &message)?;
        self.messages.push(message);
        self.active_goal = Some(goal);
        Ok(warning)
    }

    pub(crate) fn cancel_goal(&mut self) -> Result<bool, Error> {
        if self.active_goal.is_none() {
            return Ok(false);
        }
        self.session.append_goal_cancel()?;
        self.active_goal = None;
        Ok(true)
    }

    pub(super) fn clear_cancellation(&self) {
        self.cancellation.clear();
    }

    pub(super) fn set_print_mode(&mut self) {
        self.print_mode = true;
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
        self.config.enter_steers = config.enter_steers;
    }

    /// Starts a fresh session (used by `/new` and `/clear`).
    pub fn reset(&mut self) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        let old_id = self.session.id().to_string();
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
        self.active_goal = None;
        let _ = self.steers.drain();
        Checkpoints::remove(&self.config.home_dir, &old_id);
        if abandon_empty {
            let _ = Session::delete(&dirs.project, &old_id);
        }
        self.checkpoints = Some(Checkpoints::open(
            &self.config.home_dir,
            self.session.id(),
            cwd,
        ));
        Ok(())
    }

    /// Deletes a saved session. If it is the active session, starts a fresh one.
    pub fn delete_session(&mut self, id: &str) -> Result<(), Error> {
        let cwd = crate::config::working_dir();
        let dirs = self.config.session_dirs(&cwd);
        if self.session.id() == id {
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
        let id = self.session.id().to_string();
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
        let active_goal = session.active_goal().map(str::to_string);
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
        self.active_goal = active_goal;
        let _ = self.steers.drain();
        self.checkpoints = Some(Checkpoints::open(
            &self.config.home_dir,
            self.session.id(),
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
        let restore = match &mut self.checkpoints {
            Some(checkpoints) => checkpoints.restore_last()?,
            None => RestoreReport::default(),
        };
        self.session.append_undo_event(dropped, clear_goal)?;
        self.messages.truncate(start);
        if clear_goal {
            self.active_goal = None;
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
}

#[cfg(test)]
mod tests;
