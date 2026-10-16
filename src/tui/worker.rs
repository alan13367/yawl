//! Busy-mode input handling for the event loop: steering, commands and
//! picker actions that run mid-turn, pending questions, and cancellation.

use std::io::Read;

use crate::agent::{Agent, SteerInbox};
use crate::cancellation::CancellationToken;
use crate::config::{Config, ConfigChange};
use crate::error::Error;

use super::commands::{
    Command, busy_command, copy_all_from_transcript, copy_last_reply, handle_queue_picker_action,
    notice_config_effect, sync_display_settings, unqueue,
};
use super::events::{Event, EventReader, Key, MouseEvent, MouseKind};
use super::input::{EditAction, Editor, Submission};
use super::picker::{
    ActivePickers, PickerAction, SettingsCategory, SettingsItem, SettingsLocation,
    open_model_picker_from_config, open_subagent_model_picker, reasoning_picker_from_config,
    remove_model_confirm, select_picker_item, settings_item_index, status_bar_editor_picker,
    web_search_provider_picker,
};
use super::state::{
    COPY_TOAST_TICKS, ViewState, clear_jump_pointer, handle_jump_button_mouse,
    handle_scroll_bar_mouse, handle_tool_click, hover_scroll_bar, pointer_over_jump_button,
};
use super::terminal::Terminal;

pub(super) fn sync_question(
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

pub(super) fn handle_question_key(
    active: &mut super::state::ActiveQuestion,
    broker: &crate::tools::QuestionBroker,
    key: Key,
) {
    // Any key, including editing the custom answer, means the user is here.
    let _ = broker.mark_present(active.snapshot.request_id);
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
}

pub(super) fn mark_question_present(
    question: Option<&super::state::ActiveQuestion>,
    broker: &crate::tools::QuestionBroker,
) {
    if let Some(active) = question {
        let _ = broker.mark_present(active.snapshot.request_id);
    }
}

pub(super) fn paste_question_answer(state: &mut ViewState, text: &str) {
    let Some(super::state::ActiveQuestion {
        input: super::state::QuestionInput::Custom(editor),
        ..
    }) = state.question.as_mut()
    else {
        return;
    };
    editor.paste(text);
}

/// Hover state of the transcript controls: the scroll-to-bottom pill and the
/// scroll rail.
fn hovered_controls(state: &ViewState) -> (bool, bool) {
    (pointer_over_jump_button(state), state.scroll_bar_hovered)
}

/// Returns whether the hover state of the scroll-to-bottom pill or the scroll
/// rail changed. Pointer motion does not otherwise redraw.
pub(super) fn handle_mouse_selection(
    terminal: &mut Terminal,
    state: &mut ViewState,
    event: MouseEvent,
) -> Result<bool, Error> {
    let was_hovered = hovered_controls(state);
    hover_scroll_bar(state, event);
    if handle_jump_button_mouse(state, event) {
        state.tool_click_press = None;
        return Ok(hovered_controls(state) != was_hovered);
    }
    if handle_scroll_bar_mouse(state, event) {
        state.tool_click_press = None;
        return Ok(hovered_controls(state) != was_hovered);
    }
    match event.kind {
        MouseKind::Press => {
            state.tool_click_press = Some(event);
        }
        MouseKind::Release => {
            let press = state.tool_click_press.take();
            let clicked = press.is_some_and(|press| handle_tool_click(state, press, event));
            // Always let the terminal clear its pending selection so a click
            // never leaves a stale highlight behind.
            let copied = terminal.handle_mouse(event)?;
            if copied && !clicked {
                state.copy_toast_ticks = COPY_TOAST_TICKS;
            }
            return Ok(hovered_controls(state) != was_hovered);
        }
        MouseKind::Drag | MouseKind::Move => {}
    }
    if terminal.handle_mouse(event)? {
        state.copy_toast_ticks = COPY_TOAST_TICKS;
    }
    Ok(hovered_controls(state) != was_hovered)
}

/// Applies focus changes, which no view handles, and returns the next other
/// event, or `None` once nothing else is pending. The flag says whether the
/// frame needs redrawing because hover state was cleared.
pub(super) fn apply_focus_events<R: Read>(
    mut event: Event,
    events: &mut EventReader<R>,
    terminal: &mut Terminal,
    state: &mut ViewState,
) -> Result<(Option<Event>, bool), Error> {
    let mut needs_draw = false;
    loop {
        match event {
            Event::FocusGained => terminal.set_focused(true),
            Event::FocusLost => {
                terminal.set_focused(false);
                needs_draw |= super::git::clear_hover(state);
                needs_draw |= clear_jump_pointer(state);
                needs_draw |= std::mem::take(&mut state.scroll_bar_hovered);
            }
            other => return Ok((Some(other), needs_draw)),
        }
        if !events.has_pending() {
            return Ok((None, needs_draw));
        }
        event = events.read_event()?;
    }
}

/// Attaches the clipboard image to the draft when the current model accepts
/// images, or explains why it cannot.
pub(super) fn paste_clipboard_image(config: &Config, state: &mut ViewState, editor: &mut Editor) {
    let supported = crate::model::supports_images(config, &state.model);
    match editor.paste_clipboard_image(supported) {
        Ok(()) => state.scroll_offset = 0,
        Err(error) => state.notice(format!("Could not paste image: {error}.")),
    }
}

pub(super) fn is_cancel_key(key: Key) -> bool {
    matches!(key, Key::Escape | Key::Ctrl('c'))
}

pub(super) fn cancel_worker(
    thread: usize,
    cancellation: &CancellationToken,
    state: &mut ViewState,
) {
    state.queue_paused = !state.queued_inputs.is_empty() || !state.pending_steers.is_empty();
    cancellation.cancel();
    interrupt_thread(thread);
    state.transcript.mark_progress();
    state.activity = "canceling turn".into();
}

/// Gives the agent what changed while it was busy: steers it never took
/// become queued messages, and display and reasoning settings changed
/// against the busy config snapshot apply to it.
pub(super) fn hand_back_busy_changes(
    agent: &mut Agent,
    state: &mut ViewState,
    active_config: &Config,
) {
    recover_unaccepted_steers(agent, state);
    agent.sync_display_config(active_config);
    if let Err(error) = agent.set_reasoning_effort(active_config.reasoning_effort.clone()) {
        state.reasoning_effort =
            crate::model::effective_reasoning_effort(agent.config(), agent.model())
                .map(str::to_string);
        state.notice(format!("Could not save reasoning effort: {error}"));
    }
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
    if state.queue_paused && !state.queued_inputs.is_empty() {
        state.notice("Waiting messages paused. Use /unqueue and Enter to send them when ready.");
    }
}

pub(super) fn handle_steering_while_busy(
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
        let mut input = input;
        editor.expand_pastes_in_submission(&mut input);
        state.queued_inputs.push_back(input);
        state.scroll_offset = 0;
        return Ok(());
    }
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
    editor.expand_pastes_in_submission(&mut pending);
    state.pending_steers.push_back(pending);
    state.scroll_offset = 0;
    Ok(())
}

pub(super) fn handle_submission_while_busy(
    input: Submission,
    state: &mut ViewState,
    active_pickers: &ActivePickers,
    active_config: &mut Config,
    background: &crate::background::BackgroundProcessManager,
    terminal: &mut Terminal,
) -> Result<(), Error> {
    match busy_command(&input.text) {
        Some((Command::Settings, _)) => state.picker = Some(active_pickers.settings.clone()),
        Some((Command::Model, _)) => {
            let selected_model = state.model.clone();
            open_model_picker_from_config(active_config, &selected_model, state, false);
        }
        Some((Command::Reasoning, argument)) => {
            if argument.is_empty() {
                if crate::model::reasoning_efforts(active_config, &state.model).is_empty() {
                    state.notice(format!("{} does not expose reasoning levels.", state.model));
                } else {
                    state.picker = Some(reasoning_picker_from_config(
                        active_config,
                        &state.model,
                        false,
                    ));
                }
            } else {
                set_reasoning_while_busy(active_config, state, &argument);
            }
        }
        Some((Command::Connect, _)) => super::connection::open(state, active_config, false),
        Some((Command::Unqueue, argument)) => unqueue(&argument, state),
        Some((Command::Subagents, _)) => super::subagents::open_dashboard(state),
        Some((Command::Git, _)) => super::git::open_dashboard(state),
        Some((Command::Processes, _)) => {
            super::processes::open_dashboard(state, background.clone());
        }
        Some((Command::Copy, _)) => copy_last_reply(terminal, state, &[])?,
        Some((Command::CopyAll, _)) => copy_all_from_transcript(terminal, state)?,
        Some((Command::Usage, _)) => super::commands::show_usage(state),
        Some((Command::Goal, argument)) => super::commands::goal_while_busy(&argument, state),
        Some((Command::Plan, argument)) => super::commands::plan_while_busy(&argument, state),
        Some((Command::Remote, argument)) => super::remote::command(terminal, state, &argument),
        // The table only lets the commands above through while busy.
        Some(_) | None => {
            state.queued_inputs.push_back(input);
            state.scroll_offset = 0;
        }
    }
    Ok(())
}

pub(super) fn set_reasoning_while_busy(config: &mut Config, state: &mut ViewState, argument: &str) {
    let supported = crate::model::reasoning_efforts(config, &state.model);
    let effort = argument.to_ascii_lowercase();
    if effort == "default" {
        config.reasoning_effort = None;
        state.reasoning_effort = None;
    } else if supported.contains(&effort.as_str()) {
        config.reasoning_effort = Some(effort.clone());
        state.reasoning_effort = Some(effort);
    } else if supported.is_empty() {
        state.notice(format!("{} does not expose reasoning levels.", state.model));
    } else {
        state.notice(format!(
            "{} supports default, {}.",
            state.model,
            supported.join(", ")
        ));
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
            // A rejected color value never reaches `apply_display_config_while_busy`,
            // so end the preview that the closed picker left behind.
            state.end_color_preview();
            state.notice(format!("Could not change setting: {error}"));
            return;
        }
        Ok(None) => {}
    }
    match action {
        PickerAction::OpenSubagentModels => open_subagent_model_picker(active_config, state),
        PickerAction::OpenModels { save: true } => {
            let selected_model = active_config
                .model
                .as_deref()
                .unwrap_or(&state.model)
                .to_string();
            open_model_picker_from_config(active_config, &selected_model, state, true);
        }
        PickerAction::OpenModels { save: false } => {
            let selected_model = state.model.clone();
            open_model_picker_from_config(active_config, &selected_model, state, false);
        }
        PickerAction::ReturnToModels { save, selected } => {
            open_busy_model_picker(active_config, state, save, selected);
        }
        PickerAction::ConfirmRemoveModel {
            model,
            save,
            selected,
        } => match remove_model_confirm(active_config, &model, save, selected) {
            Ok(picker) => state.picker = Some(picker),
            Err(message) => {
                state.notice(message);
                open_busy_model_picker(active_config, state, save, selected);
            }
        },
        PickerAction::OpenReasoning { save: true } => {
            state.picker = Some(active_pickers.default_reasoning.clone());
        }
        PickerAction::OpenReasoning { save: false } => {
            state.picker = Some(reasoning_picker_from_config(
                active_config,
                &state.model,
                false,
            ));
        }
        PickerAction::SetReasoning {
            effort,
            save: false,
        } => {
            set_reasoning_while_busy(active_config, state, effort.as_deref().unwrap_or("default"));
        }
        PickerAction::OpenAccentColor => {
            state.begin_color_preview(active_config.selection_color);
            state.picker = Some(active_pickers.accent_color.clone());
        }
        PickerAction::OpenSelectionColor => {
            state.begin_color_preview(active_config.selection_color);
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

fn open_busy_model_picker(config: &Config, state: &mut ViewState, save: bool, selected: usize) {
    let selected_model = if save {
        config.model.as_deref().unwrap_or(&state.model).to_string()
    } else {
        state.model.clone()
    };
    open_model_picker_from_config(config, &selected_model, state, save);
    select_picker_item(state, selected);
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
        PickerAction::SetTheme(theme) => Ok(Some((
            ConfigChange::Theme(*theme),
            interface(SettingsItem::Theme),
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
    let color_change = matches!(
        change,
        ConfigChange::AccentColor(_) | ConfigChange::SelectionColor(_)
    );
    match config.change_global(change) {
        Ok(outcome) => {
            *config = outcome.config;
            sync_display_settings(config, state);
            if color_change {
                state.commit_color_preview();
            }
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
        Err(error) => {
            if color_change {
                state.end_color_preview();
            }
            state.notice(format!("Could not change setting: {error}"));
        }
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

    /// Starts a pending question with a running countdown and returns the
    /// asker handle plus the view of it the TUI would hold.
    fn pending_question(
        broker: &crate::tools::QuestionBroker,
    ) -> (
        std::thread::JoinHandle<Result<String, String>>,
        super::super::state::ActiveQuestion,
    ) {
        broker.enable();
        broker.begin_turn();
        let asker = broker.clone();
        let questions = vec![active_question().snapshot.question];
        let handle = std::thread::spawn(move || asker.ask(questions));
        while broker.snapshot().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let active =
            super::super::state::ActiveQuestion::new(broker.snapshot().expect("active question"));
        assert!(
            broker
                .snapshot()
                .expect("question pending")
                .remaining
                .is_some()
        );
        (handle, active)
    }

    fn finish_pending(
        broker: &crate::tools::QuestionBroker,
        handle: std::thread::JoinHandle<Result<String, String>>,
    ) {
        broker.cancel_pending();
        assert!(handle.join().expect("asker thread").is_err());
    }

    #[test]
    fn any_question_key_marks_the_user_present() {
        // Down changes the selection; a digit for the current row, an
        // unbound key, and Escape in the custom editor do not.
        for key in [Key::Down, Key::Char('1'), Key::Char('x'), Key::Tab] {
            let broker = crate::tools::QuestionBroker::default();
            let (handle, mut active) = pending_question(&broker);
            handle_question_key(&mut active, &broker, key);
            assert!(
                broker
                    .snapshot()
                    .expect("question should remain pending")
                    .remaining
                    .is_none(),
                "{key:?} should disable the countdown"
            );
            finish_pending(&broker, handle);
        }

        let broker = crate::tools::QuestionBroker::default();
        let (handle, mut active) = pending_question(&broker);
        active.input = super::super::state::QuestionInput::Custom(Box::default());
        handle_question_key(&mut active, &broker, Key::Escape);
        assert!(!active.is_custom());
        assert!(broker.snapshot().expect("pending").remaining.is_none());
        finish_pending(&broker, handle);

        let broker = crate::tools::QuestionBroker::default();
        let (handle, mut active) = pending_question(&broker);
        active.input = super::super::state::QuestionInput::Custom(Box::default());
        handle_question_key(&mut active, &broker, Key::Char('a'));
        assert!(broker.snapshot().expect("pending").remaining.is_none());
        finish_pending(&broker, handle);
    }

    #[test]
    fn scrolling_or_pasting_while_a_question_is_pending_marks_the_user_present() {
        let broker = crate::tools::QuestionBroker::default();
        let (handle, active) = pending_question(&broker);
        mark_question_present(Some(&active), &broker);
        assert!(broker.snapshot().expect("pending").remaining.is_none());
        finish_pending(&broker, handle);
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    #[test]
    fn reasoning_save_failure_is_reported_and_reverts_the_view() {
        for successful in [true, false] {
            let root = crate::test_support::unique_temp_dir(&format!("worker-save-{successful}"));
            let config = Config {
                home_dir: root.join("home"),
                project_dir: root.join("project/.yawl"),
                reasoning_effort: Some("low".into()),
                ..Config::test_default()
            };
            let cwd = root.join("project");
            let mut session = crate::session::Session::create(
                &config.session_dirs(&cwd).project,
                &cwd,
                "openai-codex:gpt-5.4",
            )
            .unwrap();
            session.fail_append_after(0);
            let mut agent = Agent::new(config, "openai-codex:gpt-5.4".into(), session);
            let mut state = ViewState::from_agent(&agent);
            state.reasoning_effort = Some("high".into());
            let mut active_config = agent.config().clone();
            active_config.reasoning_effort = Some("high".into());
            hand_back_busy_changes(&mut agent, &mut state, &active_config);
            assert_eq!(agent.config().reasoning_effort.as_deref(), Some("low"));
            assert_eq!(state.reasoning_effort.as_deref(), Some("low"));
            assert!(state.transcript.entries().iter().any(|entry| matches!(
                entry, super::super::transcript::Entry::Notice(message)
                    if message.contains("Could not save reasoning effort")
            )));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}
