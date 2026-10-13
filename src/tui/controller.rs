//! The TUI event loop. One loop serves an idle agent and an agent lent to a
//! scoped worker thread for a job; only submissions, picker actions, and a
//! few keys behave differently while a job runs.
//!
//! While a job runs the worker owns `&mut Agent`, so the UI cannot reach the
//! agent by construction: busy commands act on a snapshot of the config,
//! which is handed back when the job settles.

use std::io::Read;
use std::sync::mpsc::{self, Receiver};
use std::thread::{Scope, ScopedJoinHandle};

use crate::agent::{Agent, SteerInbox, TurnRequest};
use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::error::Error;

use super::commands::{
    Command, activate_picker_action, busy_command, plan_handoff_picker, promote_queued,
};
use super::completion::handle_completion_key;
use super::events::{Event, EventReader, Key, event_requests_redraw};
use super::input::{EditAction, Editor, Submission};
use super::picker::{
    ActivePickers, PickerAction, paste_into_picker, picker_is_editing, picker_is_plan_handoff,
    poll_model_picker, sync_color_preview, take_picker_action,
};
use super::state::{Update, ViewState, advance_ticks, scroll, toggle_tool_expansion};
use super::terminal::Terminal;
use super::worker::{
    activate_picker_action_while_busy, apply_focus_events, cancel_worker, hand_back_busy_changes,
    handle_mouse_selection, handle_question_key, handle_steering_while_busy,
    handle_submission_while_busy, interrupt_thread, is_cancel_key, mark_question_present,
    native_thread_id, paste_clipboard_image, paste_question_answer, sync_question,
};
use super::{
    Submitted, connection, continue_turn, git, handle_submission, navigation, notify_settled,
    processes, subagents,
};

/// Work that runs on the worker thread while the UI stays live.
pub(super) enum Job {
    /// One agent turn. `displayed` is the user text added to the transcript.
    Turn {
        displayed: Option<String>,
        request: TurnRequest,
    },
    /// `/compact`.
    Compact,
    /// A follow-up turn on subagent results that arrived while idle.
    Deferred,
}

enum JobOutcome {
    Turn(Result<bool, Error>),
    Compact(Result<(), Error>),
    Deferred(Result<Option<bool>, Error>),
}

/// A job in progress and the busy-mode view of the agent: handles the UI
/// may use without the agent, and the config snapshot busy commands change.
struct RunningJob<'scope, 'env> {
    handle: ScopedJoinHandle<'scope, (&'env mut Agent, JobOutcome)>,
    updates: Receiver<Update>,
    thread: usize,
    cancellation: CancellationToken,
    steers: SteerInbox,
    background: crate::background::BackgroundProcessManager,
    questions: crate::tools::QuestionBroker,
    active_pickers: ActivePickers,
    active_config: Config,
    /// The reasoning effort last forwarded to the worker.
    last_reasoning: Option<String>,
}

struct Controller<'scope, 'env, R> {
    scope: &'scope Scope<'scope, 'env>,
    /// `None` while a job owns the agent.
    agent: Option<&'env mut Agent>,
    job: Option<RunningJob<'scope, 'env>>,
    state: ViewState,
    editor: Editor,
    terminal: Terminal,
    events: EventReader<R>,
    needs_draw: bool,
}

/// Whether the loop keeps running after handling something.
enum Flow {
    Continue,
    Quit,
}

/// Runs the loop until `/quit`.
pub(super) fn run<R: Read>(
    agent: &mut Agent,
    state: ViewState,
    editor: Editor,
    terminal: Terminal,
    events: EventReader<R>,
) -> Result<(), Error> {
    std::thread::scope(|scope| {
        Controller {
            scope,
            agent: Some(agent),
            job: None,
            state,
            editor,
            terminal,
            events,
            needs_draw: true,
        }
        .run()
    })
}

impl<'scope, 'env, R: Read> Controller<'scope, 'env, R> {
    fn run(&mut self) -> Result<(), Error> {
        loop {
            if self.job.is_some() {
                self.sync_job_view();
                if self.job_finished() {
                    self.finish_job()?;
                    continue;
                }
            } else {
                match self.start_idle_work()? {
                    Some(Flow::Quit) => return Ok(()),
                    Some(Flow::Continue) => continue,
                    None => {}
                }
            }
            if self.needs_draw || self.terminal.size_changed() {
                self.draw()?;
            }
            let event = self.events.read_event()?;
            if let Flow::Quit = self.handle_batch(event)? {
                return Ok(());
            }
            self.forward_reasoning_change();
        }
    }

    fn draw(&mut self) -> Result<(), Error> {
        self.needs_draw = false;
        self.terminal.draw(&mut self.state, &self.editor)
    }

    /// Idle work that waits for the agent: a requested implementation,
    /// subagent results, queued picker changes, and queued messages.
    /// `None` means nothing was waiting.
    fn start_idle_work(&mut self) -> Result<Option<Flow>, Error> {
        let Some(agent) = self.agent.as_deref_mut() else {
            return Ok(None);
        };
        if self.state.pending_plan_implementation {
            self.state.pending_plan_implementation = false;
            self.start_job(Job::Turn {
                displayed: None,
                request: TurnRequest::PlanImplement(None),
            })?;
            return Ok(Some(Flow::Continue));
        }
        if agent.has_deferred_subagent_results() {
            self.start_job(Job::Deferred)?;
            return Ok(Some(Flow::Continue));
        }
        if let Some(action) = self.state.pending_actions.pop_front() {
            activate_picker_action(agent, &mut self.state, action);
            self.draw()?;
            return Ok(Some(Flow::Continue));
        }
        if self.state.picker.is_none()
            && !self.state.queue_paused
            && let Some(input) = self.state.queued_inputs.pop_front()
        {
            return self.submit(input).map(Some);
        }
        Ok(None)
    }

    /// Handles an idle submission, starting the job it asks for.
    fn submit(&mut self, input: Submission) -> Result<Flow, Error> {
        let Some(agent) = self.agent.as_deref_mut() else {
            return Ok(Flow::Continue);
        };
        match handle_submission(
            agent,
            input,
            &mut self.state,
            &mut self.editor,
            &mut self.terminal,
        )? {
            Submitted::Quit => Ok(Flow::Quit),
            Submitted::Start(job) => {
                self.start_job(job)?;
                Ok(Flow::Continue)
            }
            Submitted::Handled => Ok(Flow::Continue),
        }
    }

    fn start_job(&mut self, job: Job) -> Result<(), Error> {
        let Some(agent) = self.agent.take() else {
            return Ok(());
        };
        match &job {
            Job::Turn { displayed, request } => {
                self.state.continue_offered = false;
                if let Some(displayed) = displayed {
                    self.state.transcript.push_user(displayed.clone());
                }
                self.state.begin_turn("sending");
                self.state.sync_modes(agent);
                self.state.goal_running = matches!(request, TurnRequest::Goal);
                self.state.scroll_offset = 0;
            }
            Job::Compact => self.state.apply(Update::Compacting),
            Job::Deferred => self.state.begin_turn("delivering subagent results"),
        }
        self.draw()?;
        let active_config = agent.config().clone();
        let active_pickers = ActivePickers::from_agent(agent);
        let cancellation = agent.cancellation_token();
        let steers = agent.steer_inbox();
        let background = agent.background_processes();
        let questions = agent.question_broker();
        agent.clear_cancellation();
        let (updates_tx, updates) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        let handle = self.scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let mut sink = |event: crate::agent::TurnEvent<'_>| {
                let _ = updates_tx.send(Update::from_event(event));
            };
            let outcome = match job {
                Job::Turn { request, .. } => JobOutcome::Turn(agent.run(request, &mut sink)),
                Job::Compact => JobOutcome::Compact(agent.compact(&mut sink)),
                Job::Deferred => {
                    JobOutcome::Deferred(agent.run_deferred_subagent_results(&mut sink))
                }
            };
            (agent, outcome)
        });
        let thread = thread_rx
            .recv()
            .map_err(|_| Error::Protocol("agent worker did not start".into()))?;
        self.job = Some(RunningJob {
            handle,
            updates,
            thread,
            cancellation,
            steers,
            background,
            questions,
            last_reasoning: active_config.reasoning_effort.clone(),
            active_pickers,
            active_config,
        });
        Ok(())
    }

    /// Applies the worker's progress and the pending question.
    fn sync_job_view(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        self.needs_draw |= sync_question(&mut self.terminal, &mut self.state, &job.questions);
        while let Ok(update) = job.updates.try_recv() {
            self.state.apply(update);
            self.needs_draw = true;
        }
    }

    fn job_finished(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
    }

    /// Takes the agent back from a finished job and settles the view.
    fn finish_job(&mut self) -> Result<(), Error> {
        let Some(job) = self.job.take() else {
            return Ok(());
        };
        let (agent, outcome) = job
            .handle
            .join()
            .map_err(|_| Error::Protocol("agent worker stopped unexpectedly".into()))?;
        while let Ok(update) = job.updates.try_recv() {
            self.state.apply(update);
        }
        self.state.question = None;
        self.draw()?;
        hand_back_busy_changes(agent, &mut self.state, &job.active_config);
        match outcome {
            JobOutcome::Turn(result) => self.finish_turn(agent, result),
            JobOutcome::Compact(result) => {
                self.state.activity.clear();
                match result {
                    Ok(()) => {}
                    Err(Error::Interrupted) => self.state.notice("Compaction interrupted."),
                    Err(error) => self.state.notice(format!("Compaction failed: {error}")),
                }
            }
            JobOutcome::Deferred(result) => {
                match result {
                    Ok(Some(true) | None) => {}
                    Ok(Some(false)) | Err(Error::Interrupted) => {
                        self.state.notice("Subagent result follow-up interrupted.");
                    }
                    Err(error) => self
                        .state
                        .notice(format!("Subagent result follow-up failed: {error}")),
                }
                self.state.rebuild_transcript(agent.messages());
                self.state.activity.clear();
                self.state.turn_started = None;
                notify_settled(&mut self.terminal, &self.state);
                self.draw()?;
            }
        }
        self.agent = Some(agent);
        self.needs_draw = true;
        Ok(())
    }

    fn finish_turn(&mut self, agent: &Agent, result: Result<bool, Error>) {
        let state = &mut self.state;
        state.sync_modes(agent);
        state.transcript.finish_streaming_response();
        let completed = match result {
            Ok(true) => true,
            Ok(false) | Err(Error::Interrupted) => {
                if agent.can_continue() {
                    state.notice("Turn interrupted. Type /continue to pick it up again.");
                } else {
                    state.notice("Turn interrupted.");
                }
                false
            }
            Err(error) => {
                state.continue_offered = agent.can_continue();
                if state.continue_offered {
                    state.notice(format!(
                        "Request failed: {error}\nPress Enter to continue where it stopped, or type a new message."
                    ));
                } else {
                    state.notice(format!("Request failed: {error}"));
                }
                false
            }
        };
        crate::set_interrupted(false);
        state.activity.clear();
        state.turn_started = None;
        state.goal_running = false;
        notify_settled(&mut self.terminal, state);
        // Only a turn that ended through plan_complete offers the handoff;
        // follow-up turns replying to unrelated requests must not.
        if completed && agent.plan_ready_this_turn() {
            state.picker = Some(plan_handoff_picker());
        }
    }

    /// Hands a reasoning change made by a busy command to the worker, which
    /// applies it before its next request.
    fn forward_reasoning_change(&mut self) {
        if let Some(job) = &mut self.job
            && job.active_config.reasoning_effort != job.last_reasoning
        {
            job.last_reasoning
                .clone_from(&job.active_config.reasoning_effort);
            job.steers.set_reasoning_effort(job.last_reasoning.clone());
        }
    }

    /// Handles `event` and every event already waiting behind it.
    fn handle_batch(&mut self, mut event: Event) -> Result<Flow, Error> {
        loop {
            let (next, focus_redraw) =
                apply_focus_events(event, &mut self.events, &mut self.terminal, &mut self.state)?;
            self.needs_draw |= focus_redraw;
            let Some(next) = next else {
                return Ok(Flow::Continue);
            };
            if let Flow::Quit = self.handle_event(next)? {
                return Ok(Flow::Quit);
            }
            if !self.events.has_pending() {
                return Ok(Flow::Continue);
            }
            event = self.events.read_event()?;
        }
    }

    fn handle_event(&mut self, event: Event) -> Result<Flow, Error> {
        self.needs_draw |= event_requests_redraw(&event);
        if matches!(event, Event::Tick) {
            self.needs_draw |= self.terminal.remote().poll(&mut self.state);
            if crate::interrupted() {
                crate::set_interrupted(false);
                self.interrupt();
                self.needs_draw = true;
            }
            if let Some(job) = &self.job
                && job.cancellation.is_canceled()
            {
                // Keep waking a canceled worker until it settles: a wake that
                // lands just before it blocks in a read is otherwise lost and
                // the turn would hang on "canceling".
                interrupt_thread(job.thread);
            }
        }
        // Dashboards and modals own every event while open.
        if self.state.git_init.is_some() {
            self.needs_draw = true;
            git::handle_init_event(&mut self.state, event);
            return Ok(Flow::Continue);
        }
        if self.state.subagent_view.is_some() {
            self.needs_draw = true;
            subagents::handle_event(&mut self.state, &mut self.editor, event);
            return Ok(Flow::Continue);
        }
        if self.state.process_view.is_some() {
            self.needs_draw = true;
            processes::handle_event(&mut self.state, event);
            return Ok(Flow::Continue);
        }
        if self.state.git_view.is_some() {
            self.needs_draw = true;
            git::handle_event(&mut self.state, &mut self.editor, event);
            return Ok(Flow::Continue);
        }
        if self.job.is_some() && self.state.question.is_some() {
            self.handle_question_event(event)?;
            return Ok(Flow::Continue);
        }
        if self.state.picker.is_some() {
            return self.handle_picker_event(event);
        }
        match event {
            Event::Tick => self.poll_ticks(),
            Event::FocusGained | Event::FocusLost => {}
            Event::MouseScroll(amount) => scroll(&mut self.state, amount),
            Event::Mouse(mouse) => {
                self.needs_draw |=
                    handle_mouse_selection(&mut self.terminal, &mut self.state, mouse)?;
            }
            Event::Paste(text) => self.paste(&text),
            Event::Key(key) => return self.handle_key(key),
        }
        Ok(Flow::Continue)
    }

    fn poll_ticks(&mut self) {
        self.needs_draw |= advance_ticks(&mut self.state);
        self.needs_draw |= connection::poll(&mut self.state);
        self.needs_draw |= poll_model_picker(&mut self.state);
    }

    /// Ctrl+C from outside the TUI (SIGINT): stop a dashboard's operation,
    /// otherwise stop subagents and the running turn or clear the editor.
    fn interrupt(&mut self) {
        if git::handle_interrupt(&mut self.state) || processes::handle_interrupt(&mut self.state) {
            return;
        }
        self.state.subagent_manager.interrupt_all();
        match &self.job {
            Some(job) => {
                job.questions.cancel_pending();
                cancel_worker(job.thread, &job.cancellation, &mut self.state);
            }
            None => {
                self.editor.clear();
                self.state.activity = "input cleared".into();
            }
        }
    }

    fn handle_question_event(&mut self, event: Event) -> Result<(), Error> {
        let Some(job) = &self.job else {
            return Ok(());
        };
        match event {
            Event::Key(Key::Ctrl('c')) => {
                job.questions.cancel_pending();
                cancel_worker(job.thread, &job.cancellation, &mut self.state);
            }
            Event::Key(Key::Escape)
                if self
                    .state
                    .question
                    .as_ref()
                    .is_none_or(|question| !question.is_custom()) =>
            {
                job.questions.cancel_pending();
                cancel_worker(job.thread, &job.cancellation, &mut self.state);
            }
            Event::Key(key) => {
                if let Some(question) = self.state.question.as_mut() {
                    handle_question_key(question, &job.questions, key);
                }
            }
            Event::Tick => {
                self.needs_draw |= advance_ticks(&mut self.state);
                self.needs_draw |=
                    sync_question(&mut self.terminal, &mut self.state, &job.questions);
            }
            Event::MouseScroll(amount) => {
                mark_question_present(self.state.question.as_ref(), &job.questions);
                scroll(&mut self.state, amount);
            }
            Event::Mouse(mouse) => {
                // Scroll-bar drags and jump-button clicks scroll the
                // transcript; bare pointer motion proves nothing.
                if mouse.kind != super::events::MouseKind::Move {
                    mark_question_present(self.state.question.as_ref(), &job.questions);
                }
                self.needs_draw |=
                    handle_mouse_selection(&mut self.terminal, &mut self.state, mouse)?;
            }
            Event::Paste(text) => {
                mark_question_present(self.state.question.as_ref(), &job.questions);
                paste_question_answer(&mut self.state, &text);
            }
            Event::FocusGained | Event::FocusLost => {}
        }
        Ok(())
    }

    fn handle_picker_event(&mut self, event: Event) -> Result<Flow, Error> {
        // The plan handoff picker sits over the finished plan, which stays
        // scrollable while the agent is idle.
        let plan_handoff = self.job.is_none()
            && self
                .state
                .picker
                .as_ref()
                .is_some_and(picker_is_plan_handoff);
        match event {
            Event::Key(Key::Ctrl('l')) => self.terminal.invalidate(),
            Event::Key(Key::PageUp) if plan_handoff => scroll(&mut self.state, 10),
            Event::Key(Key::PageDown) if plan_handoff => scroll(&mut self.state, -10),
            Event::Key(key) => {
                if let Some(action) = take_picker_action(&mut self.state, &mut self.editor, key) {
                    self.activate_picker(action);
                } else {
                    sync_color_preview(&mut self.state);
                }
            }
            Event::Paste(text) if picker_is_editing(&self.state) => {
                paste_into_picker(&mut self.state, &mut self.editor, &text);
            }
            Event::MouseScroll(amount) if plan_handoff => scroll(&mut self.state, amount),
            Event::Mouse(mouse) => {
                self.needs_draw |=
                    handle_mouse_selection(&mut self.terminal, &mut self.state, mouse)?;
            }
            Event::Tick => self.poll_ticks(),
            Event::MouseScroll(_) | Event::Paste(_) | Event::FocusGained | Event::FocusLost => {}
        }
        Ok(Flow::Continue)
    }

    fn activate_picker(&mut self, action: PickerAction) {
        if let Some(agent) = self.agent.as_deref_mut() {
            activate_picker_action(agent, &mut self.state, action);
            return;
        }
        let Some(job) = &mut self.job else {
            return;
        };
        if let PickerAction::SendQueued(index) = action {
            // Sending a queued message now interrupts the running turn.
            if promote_queued(&mut self.state, index) {
                job.questions.cancel_pending();
                cancel_worker(job.thread, &job.cancellation, &mut self.state);
                self.state.queue_paused = false;
            }
        } else {
            activate_picker_action_while_busy(
                &mut self.state,
                action,
                &mut job.active_pickers,
                &mut job.active_config,
            );
        }
    }

    fn paste(&mut self, text: &str) {
        if self.state.transcript.search_active() {
            self.state.transcript.search_paste(text);
            return;
        }
        self.state.transcript.blur();
        if text.is_empty() {
            self.paste_clipboard_image();
        } else {
            self.editor.paste(text);
        }
        if self.job.is_none() {
            self.state.scroll_offset = 0;
        }
    }

    /// Attaches the clipboard image, checked against the job's config
    /// snapshot while busy.
    fn paste_clipboard_image(&mut self) {
        let config = match (&self.job, &self.agent) {
            (Some(job), _) => &job.active_config,
            (None, Some(agent)) => agent.config(),
            (None, None) => return,
        };
        paste_clipboard_image(config, &mut self.state, &mut self.editor);
    }

    fn handle_key(&mut self, key: Key) -> Result<Flow, Error> {
        if let Some(job) = &self.job
            && is_cancel_key(key)
            && !self.state.transcript.search_active()
            && !self.state.transcript.viewer_open()
        {
            cancel_worker(job.thread, &job.cancellation, &mut self.state);
            return Ok(Flow::Continue);
        }
        if navigation::handle_key(&mut self.state, &mut self.terminal, key)? {
            // Transcript search, focus, or the block viewer owns the key.
            return Ok(Flow::Continue);
        }
        let busy = self.job.is_some();
        match key {
            Key::PageUp => scroll(&mut self.state, 10),
            Key::PageDown => scroll(&mut self.state, -10),
            Key::Ctrl('c') if !busy => {
                self.editor.clear();
                self.state.activity = "input cleared".into();
            }
            Key::Ctrl('l') => self.terminal.invalidate(),
            Key::Ctrl('o') => toggle_tool_expansion(&mut self.state),
            Key::Ctrl('v') | Key::Super('v') => self.paste_clipboard_image(),
            _ if handle_completion_key(&mut self.state, &mut self.editor, key) => {
                // The completion menu consumed navigation or Tab.
            }
            Key::Tab if !busy || self.editor.is_empty() => {
                navigation::focus_transcript(&mut self.state);
            }
            Key::Enter if !busy && self.state.continue_offered && self.editor.is_empty() => {
                if let Some(agent) = self.agent.as_deref_mut()
                    && let Some(job) = continue_turn(agent, &mut self.state)
                {
                    self.start_job(job)?;
                }
            }
            // Tab with a draft queues it behind the running turn.
            Key::Tab => {
                let action = self.editor.queue();
                return self.submit_while_busy(action);
            }
            _ if busy => {
                let action = self.editor.handle_key(key);
                return self.submit_while_busy(action);
            }
            _ => match self.editor.handle_key(key) {
                EditAction::Submit(input) | EditAction::Queue(input) | EditAction::Steer(input) => {
                    return self.submit(input);
                }
                EditAction::None => {}
            },
        }
        Ok(Flow::Continue)
    }

    /// Routes a submission made while a job runs: steering, a command the
    /// table allows mid-turn, or the queue.
    fn submit_while_busy(&mut self, action: EditAction) -> Result<Flow, Error> {
        let Some(job) = &mut self.job else {
            return Ok(Flow::Continue);
        };
        let input = match action {
            EditAction::None => return Ok(Flow::Continue),
            EditAction::Steer(input)
                if !matches!(
                    busy_command(&input.text),
                    Some((Command::Reasoning | Command::Remote, _))
                ) =>
            {
                handle_steering_while_busy(
                    input,
                    &mut self.state,
                    &self.editor,
                    &job.steers,
                    &job.active_config,
                )?;
                return Ok(Flow::Continue);
            }
            EditAction::Submit(input) if !input.text.trim().starts_with('/') => {
                handle_steering_while_busy(
                    input,
                    &mut self.state,
                    &self.editor,
                    &job.steers,
                    &job.active_config,
                )?;
                return Ok(Flow::Continue);
            }
            EditAction::Steer(input) | EditAction::Submit(input) | EditAction::Queue(input) => {
                input
            }
        };
        if input.has_images() && busy_command(&input.text).is_some() {
            self.state
                .notice("Images cannot accompany commands while a turn is running.");
            self.editor.restore_submission(input);
            return Ok(Flow::Continue);
        }
        let mut input = input;
        self.editor.expand_pastes_in_submission(&mut input);
        handle_submission_while_busy(
            input,
            &mut self.state,
            &job.active_pickers,
            &mut job.active_config,
            &job.background,
            &mut self.terminal,
        )?;
        Ok(Flow::Continue)
    }
}
