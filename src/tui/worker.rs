//! Scoped agent workers, busy input handling, and cancellation.

use std::io::Read;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::{Agent, SteerInbox};
use crate::cancellation::CancellationToken;
use crate::config::{Config, ConfigChange};
use crate::error::Error;

use super::commands::{
    copy_all_from_transcript, copy_last_reply, handle_queue_picker_action, notice_config_effect,
    promote_queued, unqueue,
};
use super::completion::handle_completion_key;
use super::events::{Event, EventReader, Key, MouseEvent};
use super::input::{EditAction, Editor, Submission};
use super::picker::{
    ActivePickers, PickerAction, SettingsCategory, SettingsItem, SettingsLocation,
    picker_is_editing, select_picker_item, settings_item_index, status_bar_editor_picker,
    take_picker_action, web_search_provider_picker,
};
use super::state::{
    COPY_TOAST_TICKS, Update, ViewState, advance_ticks, handle_scroll_bar_mouse, scroll,
    toggle_tool_expansion,
};
use super::terminal::Terminal;

#[derive(Clone, Copy)]
pub(super) enum TurnKind {
    Normal,
    Init,
    Goal,
    Plan,
    PlanFollowUp,
    PlanImplement,
}

pub(super) fn turn_interactive<R: Read>(
    agent: &mut Agent,
    input: Option<crate::provider::TurnInput>,
    kind: TurnKind,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<bool, Error> {
    let result = run_agent_job_interactive(
        agent,
        state,
        editor,
        terminal,
        events,
        move |agent, sink| match kind {
            TurnKind::Normal => agent.run_turn_input_preserving_cancellation(input, sink),
            TurnKind::Init => agent.run_init_preserving_cancellation(
                input.ok_or_else(|| Error::Protocol("init input is missing".into()))?,
                sink,
            ),
            TurnKind::Goal => agent.run_goal_preserving_cancellation(sink),
            TurnKind::Plan => agent.run_plan_preserving_cancellation(sink),
            TurnKind::PlanFollowUp => agent.run_plan_follow_up_preserving_cancellation(
                input.ok_or_else(|| Error::Protocol("plan follow-up input is missing".into()))?,
                sink,
            ),
            TurnKind::PlanImplement => {
                agent.run_plan_implementation_preserving_cancellation(input, sink)
            }
        },
    );
    state.active_goal = agent.active_goal().map(str::to_string);
    state.active_plan = agent.active_plan().map(str::to_string);
    state.plan_draft = matches!(
        agent.plan_state(),
        Some(crate::session::PlanState::Draft { .. })
    );
    result
}

pub(super) fn compact_interactive<R: Read>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<(), Error> {
    run_agent_job_interactive(agent, state, editor, terminal, events, |agent, sink| {
        agent.compact_now_preserving_cancellation(sink)
    })
}

pub(super) fn deferred_subagents_interactive<R: Read>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<Option<bool>, Error> {
    run_agent_job_interactive(agent, state, editor, terminal, events, |agent, sink| {
        agent.run_deferred_subagent_results(sink)
    })
}

fn run_agent_job_interactive<R, T, F>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
    job: F,
) -> Result<T, Error>
where
    R: Read,
    T: Send,
    F: FnOnce(&mut Agent, &mut dyn FnMut(crate::agent::TurnEvent<'_>)) -> Result<T, Error> + Send,
{
    let mut active_pickers = ActivePickers::from_agent(agent);
    let mut active_config = agent.config().clone();
    let active_cancellation = agent.cancellation_token();
    let steers = agent.steer_inbox();
    let background = agent.background_processes();
    let questions = agent.question_broker();
    agent.clear_cancellation();
    let active_agent = &mut *agent;
    let result = std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = job(active_agent, &mut |event| {
                let _ = updates_tx.send(Update::from_event(event));
            });
            let _ = done_tx.send(result);
        });
        let worker_thread = thread_rx
            .recv()
            .map_err(|_| Error::Protocol("agent worker did not start".into()))?;
        pump_events(
            WorkerChannels {
                updates: updates_rx,
                done: done_rx,
                thread: worker_thread,
                cancellation: active_cancellation,
                steers,
                background,
                questions,
            },
            state,
            editor,
            terminal,
            events,
            &mut active_pickers,
            &mut active_config,
        )
    });
    recover_unaccepted_steers(agent, state);
    agent.sync_display_config(&active_config);
    result
}

pub(super) struct WorkerChannels<T> {
    updates: Receiver<Update>,
    done: Receiver<Result<T, Error>>,
    thread: usize,
    cancellation: CancellationToken,
    steers: SteerInbox,
    background: crate::background::BackgroundProcessManager,
    questions: crate::tools::QuestionBroker,
}

pub(super) fn pump_events<R: Read, T>(
    worker: WorkerChannels<T>,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
    active_pickers: &mut ActivePickers,
    active_config: &mut Config,
) -> Result<T, Error> {
    let mut needs_draw = false;
    loop {
        needs_draw |= sync_question(terminal, state, &worker.questions);
        while let Ok(update) = worker.updates.try_recv() {
            state.apply(update);
            needs_draw = true;
        }
        match worker.done.try_recv() {
            Ok(result) => {
                while let Ok(update) = worker.updates.try_recv() {
                    state.apply(update);
                }
                state.question = None;
                terminal.draw(state, editor)?;
                return result;
            }
            Err(TryRecvError::Disconnected) => {
                return Err(Error::Protocol("agent worker stopped unexpectedly".into()));
            }
            Err(TryRecvError::Empty) => {}
        }
        if needs_draw || terminal.size_changed() {
            terminal.draw(state, editor)?;
            needs_draw = false;
        }
        let mut event = events.read_event()?;
        loop {
            match event {
                Event::FocusGained => {
                    terminal.set_focused(true);
                    if !events.has_pending() {
                        break;
                    }
                    event = events.read_event()?;
                    continue;
                }
                Event::FocusLost => {
                    terminal.set_focused(false);
                    if !events.has_pending() {
                        break;
                    }
                    event = events.read_event()?;
                    continue;
                }
                _ => {}
            }
            needs_draw |= !matches!(&event, Event::Tick | Event::FocusGained | Event::FocusLost);
            if matches!(&event, Event::Tick) && crate::interrupted() {
                crate::set_interrupted(false);
                if !super::processes::handle_interrupt(state) {
                    state.subagent_manager.interrupt_all();
                    worker.questions.cancel_pending();
                    cancel_worker(worker.thread, &worker.cancellation, state);
                }
                needs_draw = true;
            }
            if state.subagent_view.is_some() {
                needs_draw = true;
                super::subagents::handle_event(state, editor, event);
                break;
            }
            if state.process_view.is_some() {
                needs_draw = true;
                super::processes::handle_event(state, event);
                break;
            }
            if state.question.is_some() {
                match event {
                    Event::Key(Key::Ctrl('c')) => {
                        worker.questions.cancel_pending();
                        cancel_worker(worker.thread, &worker.cancellation, state);
                    }
                    Event::Key(Key::Escape)
                        if state
                            .question
                            .as_ref()
                            .is_none_or(|question| !question.is_custom()) =>
                    {
                        worker.questions.cancel_pending();
                        cancel_worker(worker.thread, &worker.cancellation, state);
                    }
                    Event::Key(key) => {
                        if let Some(question) = state.question.as_mut() {
                            handle_question_key(question, &worker.questions, key);
                        }
                    }
                    Event::Tick => {
                        needs_draw |= advance_ticks(state);
                        needs_draw |= sync_question(terminal, state, &worker.questions);
                    }
                    Event::MouseScroll(amount) => scroll(state, amount),
                    Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
                    Event::Paste(text) => paste_question_answer(state, &text),
                    Event::FocusGained | Event::FocusLost => {}
                }
                break;
            }
            if state.picker.is_some() {
                match event {
                    Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
                    Event::Key(key) => {
                        if let Some(action) = take_picker_action(state, editor, key) {
                            if let PickerAction::SendQueued(index) = action {
                                if promote_queued(state, index) {
                                    worker.questions.cancel_pending();
                                    cancel_worker(worker.thread, &worker.cancellation, state);
                                }
                            } else {
                                activate_picker_action_while_busy(
                                    state,
                                    action,
                                    active_pickers,
                                    active_config,
                                );
                            }
                        }
                    }
                    Event::Paste(text) if picker_is_editing(state) => editor.paste(&text),
                    Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
                    Event::Tick => {
                        needs_draw |= advance_ticks(state);
                        needs_draw |= super::connection::poll(state);
                    }
                    Event::MouseScroll(_)
                    | Event::Paste(_)
                    | Event::FocusGained
                    | Event::FocusLost => {}
                }
                break;
            }
            match event {
                Event::Tick => {
                    needs_draw |= advance_ticks(state);
                    needs_draw |= super::connection::poll(state);
                }
                Event::FocusGained | Event::FocusLost => {}
                Event::MouseScroll(amount) => scroll(state, amount),
                Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
                Event::Paste(text) => {
                    if state.transcript.search_active() {
                        state.transcript.search_paste(&text);
                    } else {
                        state.transcript.blur();
                        if text.is_empty() {
                            let supported =
                                crate::model::supports_images(active_config, &state.model);
                            match editor.paste_clipboard_image(supported) {
                                Ok(()) => state.scroll_offset = 0,
                                Err(error) => {
                                    state.notice(format!("Could not paste image: {error}."))
                                }
                            }
                        } else {
                            editor.paste(&text);
                        }
                    }
                }
                Event::Key(key)
                    if is_cancel_key(key)
                        && !state.transcript.search_active()
                        && !state.transcript.viewer_open() =>
                {
                    cancel_worker(worker.thread, &worker.cancellation, state)
                }
                Event::Key(key) => {
                    if super::navigation::handle_key(state, terminal, key)? {
                        // Transcript navigation owns the key.
                    } else {
                        match key {
                            Key::Ctrl('l') => terminal.invalidate(),
                            Key::Ctrl('o') => toggle_tool_expansion(state),
                            Key::PageUp => scroll(state, 10),
                            Key::PageDown => scroll(state, -10),
                            Key::Ctrl('v') | Key::Super('v') => {
                                let supported =
                                    crate::model::supports_images(active_config, &state.model);
                                match editor.paste_clipboard_image(supported) {
                                    Ok(()) => state.scroll_offset = 0,
                                    Err(error) => {
                                        state.notice(format!("Could not paste image: {error}."))
                                    }
                                }
                            }
                            _ if handle_completion_key(state, editor, key) => {
                                // Keep accepting and completing input while the agent runs.
                            }
                            Key::Tab if editor.is_empty() => {
                                super::navigation::focus_transcript(state)
                            }
                            _ => match if key == Key::Tab {
                                editor.queue()
                            } else {
                                editor.handle_key(key)
                            } {
                                EditAction::Steer(input) => {
                                    handle_steering_while_busy(
                                        input,
                                        state,
                                        editor,
                                        &worker.steers,
                                        active_config,
                                    )?;
                                }
                                EditAction::Submit(input)
                                    if !input.text.trim().starts_with('/') =>
                                {
                                    handle_steering_while_busy(
                                        input,
                                        state,
                                        editor,
                                        &worker.steers,
                                        active_config,
                                    )?;
                                }
                                EditAction::Submit(input) | EditAction::Queue(input) => {
                                    if input.has_images() && busy_command(&input.text).is_some() {
                                        state.notice("Images cannot accompany commands while a turn is running.");
                                        editor.restore_submission(input);
                                        continue;
                                    }
                                    let displayed = super::displayed_submission(editor, &input);
                                    let mut input = input;
                                    input.set_text(displayed);
                                    handle_submission_while_busy(
                                        input,
                                        state,
                                        active_pickers,
                                        active_config,
                                        &worker.background,
                                        terminal,
                                    )?;
                                }
                                EditAction::None => {}
                            },
                        }
                    }
                }
            }
            if !events.has_pending() {
                break;
            }
            event = events.read_event()?;
        }
    }
}

fn sync_question(
    terminal: &mut Terminal,
    state: &mut ViewState,
    broker: &crate::tools::QuestionBroker,
) -> bool {
    let snapshot = broker.snapshot();
    match (state.question.as_mut(), snapshot) {
        (Some(active), Some(snapshot))
            if active.snapshot.request_id == snapshot.request_id
                && active.snapshot.question_index == snapshot.question_index =>
        {
            let changed = active
                .snapshot
                .remaining
                .map(|remaining| remaining.as_secs())
                != snapshot.remaining.map(|remaining| remaining.as_secs());
            active.snapshot = snapshot;
            changed
        }
        (previous, Some(snapshot)) => {
            if previous.is_none() && state.bell {
                terminal.ring_bell();
            }
            state.question = Some(super::state::ActiveQuestion::new(snapshot));
            true
        }
        (Some(_), None) => {
            state.question = None;
            true
        }
        (None, None) => false,
    }
}

fn handle_question_key(
    active: &mut super::state::ActiveQuestion,
    broker: &crate::tools::QuestionBroker,
    key: Key,
) {
    if let super::state::QuestionInput::Custom(editor) = &mut active.input {
        if key == Key::Escape {
            active.input = super::state::QuestionInput::Choices;
            return;
        }
        if let EditAction::Submit(answer) = editor.handle_key(key) {
            let _ = broker.answer_custom(active.snapshot.request_id, answer.text);
        }
        return;
    }

    let model_option_count = active.snapshot.question.options.len();
    let option_count = model_option_count.saturating_add(1);
    let previous_selection = active.selected;
    match key {
        Key::Up | Key::Char('k') => {
            active.selected = active.selected.saturating_sub(1);
        }
        Key::Down | Key::Char('j') => {
            active.selected = (active.selected + 1).min(option_count.saturating_sub(1));
        }
        Key::Char(digit @ '1'..='4') => {
            let selected = usize::from(digit as u8 - b'1');
            if selected < option_count {
                active.selected = selected;
            }
        }
        Key::Enter => {
            if active.selected == model_option_count {
                active.input = super::state::QuestionInput::Custom(Box::default());
            } else {
                let _ = broker.answer(active.snapshot.request_id, active.selected);
            }
        }
        _ => {}
    }
    if active.selected != previous_selection {
        let _ = broker.mark_present(active.snapshot.request_id);
    }
}

fn paste_question_answer(state: &mut ViewState, text: &str) {
    let Some(super::state::ActiveQuestion {
        input: super::state::QuestionInput::Custom(editor),
        ..
    }) = state.question.as_mut()
    else {
        return;
    };
    editor.paste(text);
}

pub(super) fn handle_mouse_selection(
    terminal: &mut Terminal,
    state: &mut ViewState,
    event: MouseEvent,
) -> Result<(), Error> {
    if handle_scroll_bar_mouse(state, event) {
        return Ok(());
    }
    if terminal.handle_mouse(event)? {
        state.copy_toast_ticks = COPY_TOAST_TICKS;
    }
    Ok(())
}

pub(super) fn is_cancel_key(key: Key) -> bool {
    matches!(key, Key::Escape | Key::Ctrl('c'))
}

pub(super) fn cancel_worker(
    thread: usize,
    cancellation: &CancellationToken,
    state: &mut ViewState,
) {
    cancellation.cancel();
    interrupt_thread(thread);
    state.activity = "canceling turn".into();
}

fn recover_unaccepted_steers(agent: &Agent, state: &mut ViewState) {
    let leftover = agent.take_unaccepted_steers();
    let mut recovered = std::mem::take(&mut state.pending_steers);
    if recovered.len() < leftover.len() {
        for input in leftover.into_iter().skip(recovered.len()) {
            recovered.push_back(input.text.into());
        }
    }
    state.queued_inputs.extend(recovered);
}

fn handle_steering_while_busy(
    input: Submission,
    state: &mut ViewState,
    editor: &Editor,
    steers: &SteerInbox,
    active_config: &Config,
) -> Result<(), Error> {
    if input.text.trim().starts_with('/') {
        state.notice(
            "Commands cannot be sent as steering. They stay queued or run as usual with Enter.",
        );
        let displayed = super::displayed_submission(editor, &input);
        let mut input = input;
        input.set_text(displayed);
        state.queued_inputs.push_back(input);
        state.scroll_offset = 0;
        return Ok(());
    }
    let displayed = super::displayed_submission(editor, &input);
    let agent_text = editor.expand_submission(&input.text);
    let agent_input = match input.turn_input(agent_text) {
        Ok(input) => input,
        Err(error) => {
            state.notice(format!("Could not prepare images: {error}."));
            return Ok(());
        }
    };
    if !agent_input.images.is_empty() && !crate::model::supports_images(active_config, &state.model)
    {
        state.notice(format!(
            "Model '{}' does not accept image input.",
            state.model
        ));
        return Ok(());
    }
    steers.push(agent_input);
    let mut pending = input;
    pending.set_text(displayed);
    state.pending_steers.push_back(pending);
    state.scroll_offset = 0;
    Ok(())
}

pub(super) fn handle_submission_while_busy(
    input: Submission,
    state: &mut ViewState,
    active_pickers: &ActivePickers,
    active_config: &Config,
    background: &crate::background::BackgroundProcessManager,
    terminal: &mut Terminal,
) -> Result<(), Error> {
    match busy_command(&input.text) {
        Some(BusyCommand::Settings) => state.picker = Some(active_pickers.settings.clone()),
        Some(BusyCommand::Model) => state.picker = Some(active_pickers.model.clone()),
        Some(BusyCommand::Connect) => super::connection::open(state, active_config, false),
        Some(BusyCommand::Unqueue(argument)) => unqueue(&argument, state),
        Some(BusyCommand::Subagents) => super::subagents::open_dashboard(state),
        Some(BusyCommand::Processes) => super::processes::open_dashboard(state, background.clone()),
        Some(BusyCommand::Copy) => copy_last_reply(terminal, state, &[])?,
        Some(BusyCommand::CopyAll) => copy_all_from_transcript(terminal, state)?,
        Some(BusyCommand::Usage) => super::commands::show_usage(state),
        Some(BusyCommand::Goal(argument)) => super::commands::goal_while_busy(&argument, state),
        Some(BusyCommand::Plan(argument)) => super::commands::plan_while_busy(&argument, state),
        None => {
            state.queued_inputs.push_back(input);
            state.scroll_offset = 0;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BusyCommand {
    Settings,
    Model,
    Connect,
    Unqueue(String),
    Subagents,
    Processes,
    Copy,
    CopyAll,
    Usage,
    Goal(String),
    Plan(String),
}

pub(super) fn busy_command(input: &str) -> Option<BusyCommand> {
    let command = input.trim().strip_prefix('/')?;
    let (name, argument) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(name, argument)| (name, argument.trim()));
    match name {
        "settings" if argument.is_empty() => Some(BusyCommand::Settings),
        "model" if argument.is_empty() => Some(BusyCommand::Model),
        "connect" if argument.is_empty() => Some(BusyCommand::Connect),
        "unqueue" => Some(BusyCommand::Unqueue(argument.to_string())),
        "subagents" if argument.is_empty() => Some(BusyCommand::Subagents),
        "ps" if argument.is_empty() => Some(BusyCommand::Processes),
        "copy" if argument.is_empty() => Some(BusyCommand::Copy),
        "copy-all" if argument.is_empty() => Some(BusyCommand::CopyAll),
        "usage" if argument.is_empty() => Some(BusyCommand::Usage),
        "goal" => Some(BusyCommand::Goal(argument.to_string())),
        "plan" => Some(BusyCommand::Plan(argument.to_string())),
        _ => None,
    }
}

pub(super) fn activate_picker_action_while_busy(
    state: &mut ViewState,
    action: PickerAction,
    active_pickers: &mut ActivePickers,
    active_config: &mut Config,
) {
    let Some(action) = handle_queue_picker_action(state, action) else {
        return;
    };
    if let PickerAction::OpenConnect { from_settings } = action {
        super::connection::open(state, active_config, from_settings);
        return;
    }
    let Some(action) = super::connection::handle_action(state, action) else {
        return;
    };
    let Some(action) = super::status_bar::handle_editor_action(state, action) else {
        return;
    };
    let Some(action) =
        handle_status_bar_action_while_busy(state, action, active_pickers, active_config)
    else {
        return;
    };
    match display_config_change(&action) {
        Ok(Some((change, selected))) => {
            apply_display_config_while_busy(active_config, state, active_pickers, change, selected);
            return;
        }
        Err(error) => {
            state.notice(format!("Could not change setting: {error}"));
            return;
        }
        Ok(None) => {}
    }
    match action {
        PickerAction::OpenModels { save: true } => {
            state.picker = Some(active_pickers.default_model.clone());
        }
        PickerAction::OpenModels { save: false } => {
            state.picker = Some(active_pickers.model.clone());
        }
        PickerAction::OpenReasoning { save: true } => {
            state.picker = Some(active_pickers.default_reasoning.clone());
        }
        PickerAction::OpenReasoning { save: false } => {
            state.picker = Some(active_pickers.reasoning.clone());
        }
        PickerAction::OpenAccentColor => {
            state.picker = Some(active_pickers.accent_color.clone());
        }
        PickerAction::OpenSelectionColor => {
            state.picker = Some(active_pickers.selection_color.clone());
        }
        PickerAction::OpenWebSearchProviders => {
            state.picker = Some(web_search_provider_picker(
                active_config.web_search_provider,
            ));
        }
        PickerAction::OpenSettingsRoot { selected } => {
            state.picker = Some(active_pickers.settings.clone());
            select_picker_item(state, selected);
        }
        PickerAction::OpenSettingsCategory { category, selected } => {
            state.picker = active_pickers
                .settings_categories
                .iter()
                .find(|(candidate, _)| *candidate == category)
                .map(|(_, picker)| {
                    let mut picker = picker.clone();
                    picker.selected = selected.min(picker.items.len().saturating_sub(1));
                    picker
                });
        }
        PickerAction::EditSetting { .. }
        | PickerAction::EditModel { .. }
        | PickerAction::EditStatusBarLabel { .. }
        | PickerAction::EditStatusBarSeparator(_) => {}
        PickerAction::SendQueued(_)
        | PickerAction::ApplyQueued { .. }
        | PickerAction::MoveQueued { .. }
        | PickerAction::RemoveQueued(_)
        | PickerAction::ClearQueued => {}
        action => {
            state.pending_actions.push_back(action);
            state.activity = "change queued until the active response finishes".into();
        }
    }
}

fn handle_status_bar_action_while_busy(
    state: &mut ViewState,
    action: PickerAction,
    active_pickers: &mut ActivePickers,
    active_config: &mut Config,
) -> Option<PickerAction> {
    let interface_picker = |state: &mut ViewState, active_pickers: &ActivePickers| {
        state.picker = active_pickers
            .settings_categories
            .iter()
            .find(|(category, _)| *category == SettingsCategory::Interface)
            .map(|(_, picker)| {
                let mut picker = picker.clone();
                picker.selected =
                    settings_item_index(SettingsCategory::Interface, SettingsItem::StatusBar);
                picker
            });
    };
    match action {
        PickerAction::CancelStatusBarEditor => {
            state.status_bar_draft = None;
            interface_picker(state, active_pickers);
        }
        PickerAction::SaveStatusBar => {
            let Some(layout) = state.status_bar_draft.take() else {
                interface_picker(state, active_pickers);
                return None;
            };
            match active_config.change_global(ConfigChange::StatusBar(layout.clone())) {
                Ok(outcome) => {
                    *active_config = outcome.config;
                    state.status_bar = active_config.status_bar.clone();
                    notice_config_effect(active_config, outcome.effect, state);
                    active_pickers.refresh_display_settings(active_config);
                    interface_picker(state, active_pickers);
                }
                Err(error) => {
                    state.notice(format!("Could not change setting: {error}"));
                    state.status_bar_draft = Some(layout);
                    state.picker = Some(status_bar_editor_picker(state, 0));
                }
            }
        }
        action => return Some(action),
    }
    None
}

pub(super) fn display_config_change(
    action: &PickerAction,
) -> Result<Option<(ConfigChange, SettingsLocation)>, Error> {
    let interface = |item| SettingsLocation {
        category: SettingsCategory::Interface,
        item,
    };
    match action {
        PickerAction::SetHideReasoning(enabled) => Ok(Some((
            ConfigChange::HideReasoning(*enabled),
            interface(SettingsItem::ReasoningDisplay),
        ))),
        PickerAction::SetAccentColor(color) => Ok(Some((
            ConfigChange::AccentColor(*color),
            interface(SettingsItem::AccentColor),
        ))),
        PickerAction::SetSelectionColor(selection) => Ok(Some((
            ConfigChange::SelectionColor(*selection),
            interface(SettingsItem::SelectionColor),
        ))),
        PickerAction::SetScrollBar(enabled) => Ok(Some((
            ConfigChange::ScrollBar(*enabled),
            interface(SettingsItem::ScrollBar),
        ))),
        PickerAction::SetScrollBarAutoHide(enabled) => Ok(Some((
            ConfigChange::ScrollBarAutoHide(*enabled),
            interface(SettingsItem::ScrollBarAutoHide),
        ))),
        PickerAction::SetBell(enabled) => Ok(Some((
            ConfigChange::Bell(*enabled),
            interface(SettingsItem::Bell),
        ))),
        PickerAction::ApplySetting { argument, location } => {
            let change = if let Some(value) = argument.strip_prefix("accent_color ") {
                Some(ConfigChange::AccentColor(
                    crate::config::UiColor::parse(value).map_err(Error::Config)?,
                ))
            } else if let Some(value) = argument.strip_prefix("selection_color ") {
                Some(ConfigChange::SelectionColor(
                    crate::config::UiColor::parse_selection(value).map_err(Error::Config)?,
                ))
            } else {
                None
            };
            Ok(change.and_then(|change| location.map(|location| (change, location))))
        }
        _ => Ok(None),
    }
}

pub(super) fn apply_display_config_while_busy(
    config: &mut Config,
    state: &mut ViewState,
    active_pickers: &mut ActivePickers,
    change: ConfigChange,
    location: SettingsLocation,
) {
    match config.change_global(change) {
        Ok(outcome) => {
            *config = outcome.config;
            state.hide_reasoning = config.hide_reasoning;
            state.accent_color = config.accent_color;
            state.selection_color = config.effective_selection_color();
            state.sync_scroll_bar_config(config);
            state.bell = config.bell;
            state.subagents_enabled = config.subagents;
            notice_config_effect(config, outcome.effect, state);
            active_pickers.refresh_display_settings(config);
            state.picker = active_pickers
                .settings_categories
                .iter()
                .find(|(category, _)| *category == location.category)
                .map(|(_, picker)| {
                    let mut picker = picker.clone();
                    picker.selected = settings_item_index(location.category, location.item)
                        .min(picker.items.len().saturating_sub(1));
                    picker
                });
        }
        Err(error) => state.notice(format!("Could not change setting: {error}")),
    }
}

pub(super) fn native_thread_id() -> usize {
    crate::cancellation::native_thread_id()
}

pub(super) fn interrupt_thread(thread: usize) {
    crate::cancellation::wake_thread(thread);
}

#[cfg(test)]
mod question_tests {
    use super::*;

    fn active_question() -> super::super::state::ActiveQuestion {
        super::super::state::ActiveQuestion::new(crate::tools::QuestionSnapshot {
            request_id: 1,
            question_index: 0,
            question_count: 1,
            question: crate::tools::UserQuestion {
                id: "scope".into(),
                question: "Which scope?".into(),
                options: vec![
                    crate::tools::QuestionOption {
                        label: "Focused".into(),
                        description: "small".into(),
                    },
                    crate::tools::QuestionOption {
                        label: "Broad".into(),
                        description: "large".into(),
                    },
                ],
                recommended: 0,
            },
            remaining: Some(std::time::Duration::from_secs(30)),
        })
    }

    #[test]
    fn other_choice_opens_editor_and_escape_returns_to_choices() {
        let broker = crate::tools::QuestionBroker::default();
        let mut active = active_question();

        handle_question_key(&mut active, &broker, Key::Char('3'));
        assert_eq!(active.selected, 2);
        handle_question_key(&mut active, &broker, Key::Enter);
        assert!(active.is_custom());
        handle_question_key(&mut active, &broker, Key::Char('o'));
        handle_question_key(&mut active, &broker, Key::Char('w'));
        let super::super::state::QuestionInput::Custom(editor) = &active.input else {
            panic!("custom editor should stay open");
        };
        assert_eq!(editor.text(), "ow");

        handle_question_key(&mut active, &broker, Key::Escape);
        assert!(!active.is_custom());
        assert_eq!(active.selected, 2);
    }

    #[test]
    fn changing_the_selected_option_marks_the_user_present() {
        let broker = crate::tools::QuestionBroker::default();
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let questions = vec![active_question().snapshot.question];
        let handle = std::thread::spawn(move || asker.ask(questions));
        while broker.snapshot().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut active =
            super::super::state::ActiveQuestion::new(broker.snapshot().expect("active question"));

        handle_question_key(&mut active, &broker, Key::Down);
        assert_eq!(active.selected, 1);
        assert!(
            broker
                .snapshot()
                .expect("question should remain pending")
                .remaining
                .is_none()
        );

        broker.cancel_pending();
        assert!(handle.join().expect("asker thread").is_err());
    }
}
