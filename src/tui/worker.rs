//! Scoped agent workers, busy input handling, and cancellation.

use std::io::Read;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::Agent;
use crate::cancellation::CancellationToken;
use crate::config::{Config, ConfigChange};
use crate::error::Error;

use super::commands::{handle_queue_picker_action, notice_config_effect, unqueue};
use super::completion::handle_completion_key;
use super::events::{Event, EventReader, Key, MouseEvent};
use super::input::{EditAction, Editor};
use super::picker::{
    ActivePickers, PickerAction, SETTINGS_ACCENT_COLOR_INDEX, SETTINGS_REASONING_DISPLAY_INDEX,
    SETTINGS_SCROLL_BAR_INDEX, picker_is_editing, select_picker_item, take_picker_action,
};
use super::state::{
    COPY_TOAST_TICKS, Update, ViewState, advance_ticks, handle_scroll_bar_mouse, scroll,
    toggle_tool_expansion,
};
use super::terminal::Terminal;

pub(super) fn turn_interactive<R: Read>(
    agent: &mut Agent,
    input: String,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<bool, Error> {
    let mut active_pickers = ActivePickers::from_agent(agent);
    let mut active_config = agent.config().clone();
    let active_cancellation = agent.cancellation_token();
    agent.clear_cancellation();
    let active_agent = &mut *agent;
    let result = std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = active_agent.run_turn_preserving_cancellation(Some(input), &mut |event| {
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
            },
            state,
            editor,
            terminal,
            events,
            &mut active_pickers,
            &mut active_config,
        )
    });
    agent.sync_display_config(&active_config);
    result
}

pub(super) fn compact_interactive<R: Read>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<(), Error> {
    let mut active_pickers = ActivePickers::from_agent(agent);
    let mut active_config = agent.config().clone();
    let active_cancellation = agent.cancellation_token();
    agent.clear_cancellation();
    let active_agent = &mut *agent;
    let result = std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = active_agent.compact_now_preserving_cancellation(&mut |event| {
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
            },
            state,
            editor,
            terminal,
            events,
            &mut active_pickers,
            &mut active_config,
        )
    });
    agent.sync_display_config(&active_config);
    result
}

pub(super) fn deferred_subagents_interactive<R: Read>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<Option<bool>, Error> {
    let mut active_pickers = ActivePickers::from_agent(agent);
    let mut active_config = agent.config().clone();
    let active_cancellation = agent.cancellation_token();
    agent.clear_cancellation();
    let active_agent = &mut *agent;
    let result = std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = active_agent.run_deferred_subagent_results(&mut |event| {
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
            },
            state,
            editor,
            terminal,
            events,
            &mut active_pickers,
            &mut active_config,
        )
    });
    agent.sync_display_config(&active_config);
    result
}

pub(super) struct WorkerChannels<T> {
    updates: Receiver<Update>,
    done: Receiver<Result<T, Error>>,
    thread: usize,
    cancellation: CancellationToken,
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
    loop {
        while let Ok(update) = worker.updates.try_recv() {
            state.apply(update);
        }
        match worker.done.try_recv() {
            Ok(result) => {
                while let Ok(update) = worker.updates.try_recv() {
                    state.apply(update);
                }
                terminal.draw(state, editor)?;
                return result;
            }
            Err(TryRecvError::Disconnected) => {
                return Err(Error::Protocol("agent worker stopped unexpectedly".into()));
            }
            Err(TryRecvError::Empty) => {}
        }
        terminal.draw(state, editor)?;
        let event = events.read_event()?;
        if matches!(&event, Event::Tick) && crate::interrupted() {
            state.subagent_manager.interrupt_all();
            cancel_worker(worker.thread, &worker.cancellation, state);
            crate::set_interrupted(false);
        }
        if state.subagent_view.is_some() {
            super::subagents::handle_event(state, editor, event);
            continue;
        }
        if state.picker.is_some() {
            match event {
                Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
                Event::Key(key) => {
                    if let Some(action) = take_picker_action(state, editor, key) {
                        activate_picker_action_while_busy(
                            state,
                            action,
                            active_pickers,
                            active_config,
                        );
                    }
                }
                Event::Paste(text) if picker_is_editing(state) => editor.paste(&text),
                Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
                Event::Tick => advance_ticks(state),
                Event::MouseScroll(_) | Event::Paste(_) => {}
            }
            continue;
        }
        match event {
            Event::Tick => {
                advance_ticks(state);
            }
            Event::MouseScroll(amount) => scroll(state, amount),
            Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
            Event::Paste(text) => editor.paste(&text),
            Event::Key(key) if is_cancel_key(key) => {
                cancel_worker(worker.thread, &worker.cancellation, state)
            }
            Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
            Event::Key(Key::Ctrl('o')) => toggle_tool_expansion(state),
            Event::Key(Key::PageUp) => scroll(state, 10),
            Event::Key(Key::PageDown) => scroll(state, -10),
            Event::Key(key) => {
                if handle_completion_key(state, editor, key) {
                    // Keep accepting and completing input while the agent runs.
                } else if let EditAction::Submit(input) = editor.handle_key(key) {
                    handle_submission_while_busy(input, state, active_pickers);
                }
            }
        }
    }
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

pub(super) fn handle_submission_while_busy(
    input: String,
    state: &mut ViewState,
    active_pickers: &ActivePickers,
) {
    match busy_command(&input) {
        Some(BusyCommand::Settings) => state.picker = Some(active_pickers.settings.clone()),
        Some(BusyCommand::Model) => state.picker = Some(active_pickers.model.clone()),
        Some(BusyCommand::Unqueue(argument)) => unqueue(&argument, state),
        Some(BusyCommand::Subagents) => super::subagents::open_dashboard(state),
        None => {
            state.queued_inputs.push_back(input);
            state.scroll_offset = 0;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum BusyCommand {
    Settings,
    Model,
    Unqueue(String),
    Subagents,
}

pub(super) fn busy_command(input: &str) -> Option<BusyCommand> {
    let command = input.trim().strip_prefix('/')?;
    let (name, argument) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(name, argument)| (name, argument.trim()));
    match name {
        "settings" if argument.is_empty() => Some(BusyCommand::Settings),
        "model" if argument.is_empty() => Some(BusyCommand::Model),
        "unqueue" => Some(BusyCommand::Unqueue(argument.to_string())),
        "subagents" if argument.is_empty() => Some(BusyCommand::Subagents),
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
    if let Some((change, selected)) = display_config_change(&action) {
        apply_display_config_while_busy(active_config, state, active_pickers, change, selected);
        return;
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
        PickerAction::EditSetting { .. } | PickerAction::EditModel { .. } => {}
        PickerAction::RemoveQueued(_) | PickerAction::ClearQueued => {}
        action => {
            state.pending_actions.push_back(action);
            state.activity = "change queued until the active response finishes".into();
        }
    }
}

pub(super) fn display_config_change(action: &PickerAction) -> Option<(ConfigChange, usize)> {
    match action {
        PickerAction::SetHideReasoning(enabled) => Some((
            ConfigChange::HideReasoning(if *enabled { "on" } else { "off" }.into()),
            SETTINGS_REASONING_DISPLAY_INDEX,
        )),
        PickerAction::SetAccentColor(color) => Some((
            ConfigChange::AccentColor(color.config_value()),
            SETTINGS_ACCENT_COLOR_INDEX,
        )),
        PickerAction::SetScrollBar(enabled) => Some((
            ConfigChange::ScrollBar(if *enabled { "on" } else { "off" }.into()),
            SETTINGS_SCROLL_BAR_INDEX,
        )),
        PickerAction::ApplySetting { argument, selected } => argument
            .strip_prefix("accent_color ")
            .map(|value| (ConfigChange::AccentColor(value.to_string()), *selected)),
        _ => None,
    }
}

pub(super) fn apply_display_config_while_busy(
    config: &mut Config,
    state: &mut ViewState,
    active_pickers: &mut ActivePickers,
    change: ConfigChange,
    selected: usize,
) {
    match config.change_global(change) {
        Ok(outcome) => {
            *config = outcome.config;
            state.hide_reasoning = config.hide_reasoning;
            state.accent_color = config.accent_color;
            state.show_scroll_bar = config.scroll_bar;
            notice_config_effect(config, outcome.effect, state);
            active_pickers.refresh_display_settings(config);
            state.picker = Some(active_pickers.settings.clone());
            select_picker_item(state, selected);
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
