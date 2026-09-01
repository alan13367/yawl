//! Full-screen terminal UI built directly on termios and ANSI escape
//! sequences. The terminal remains responsive while the blocking agent loop
//! runs on a scoped worker thread.

mod clipboard;
mod commands;
#[cfg(test)]
mod commands_tests;
mod completion;
#[cfg(test)]
mod completion_tests;
mod connection;
mod dashboard;
pub mod events;
mod files;
pub mod highlight;
pub mod input;
pub mod markdown;
mod navigation;
mod picker;
#[cfg(test)]
mod picker_tests;
mod processes;
mod render;
#[cfg(test)]
mod render_tests;
mod search;
mod state;
#[cfg(test)]
mod state_tests;
mod subagents;
mod terminal;
#[cfg(test)]
mod terminal_tests;
mod tool_view;
mod transcript;
mod worker;
#[cfg(test)]
mod worker_tests;

use std::io::{self, Read};

use crate::agent::Agent;
use crate::error::Error;

use self::commands::{
    GoalAction, HELP, activate_picker_action, copy_all_messages, copy_last_reply, goal,
    is_new_session_command, notice_undo, open_resume_picker, resume, settings, show_skills,
    unqueue,
};
use self::completion::handle_completion_key;
use self::events::{Event, EventReader, Key};
use self::input::{EditAction, Editor, Submission};
use self::picker::{
    open_model_picker, open_reasoning_picker, open_settings_picker, picker_is_editing,
    take_picker_action,
};
use self::state::{Update, ViewState, advance_ticks, scroll, toggle_tool_expansion};
use self::subagents::open_dashboard as open_subagent_dashboard;
use self::terminal::Terminal;
use self::transcript::Transcript;
use self::worker::{
    compact_interactive, deferred_subagents_interactive, handle_mouse_selection, turn_interactive,
};

#[cfg(test)]
use self::commands::{
    format_copy_all, handle_queue_picker_action, last_assistant_reply, open_queue_picker,
};
#[cfg(test)]
use self::completion::{
    COMPLETION_MENU_ROWS, Completion, completion_window, matching_completions,
    sync_completion_filter,
};
#[cfg(test)]
use self::picker::{
    ActivePickers, Picker, PickerAction, PickerItem, color_picker, render_picker,
    selection_color_picker,
};
#[cfg(test)]
use self::render::{
    HIDDEN_CURSOR, RenderCache, WELCOME_ANIMATION_TICKS, build_frame, render_entries,
    render_loading_state, render_queued_panel, selected_row, selection_style,
};
#[cfg(test)]
use self::terminal::{
    ScreenPoint, TextSelection, cursor_control, highlighted_selection, selected_text,
};
#[cfg(test)]
use self::transcript::{Entry, TranscriptEvent};
#[cfg(test)]
use self::worker::{BusyCommand, activate_picker_action_while_busy, busy_command, is_cancel_key};
#[cfg(test)]
use crate::config::{Config, UiColor};
#[cfg(test)]
use crate::provider::ReasoningKind;

const USER_BACKGROUND: &str = "\x1b[48;2;52;53;64m";
const USER_TEXT: &str = "\x1b[38;2;208;208;214m";

/// Runs the alternate-screen terminal interface until `/quit`.
///
/// # Errors
///
/// Returns terminal setup, rendering, session, or input errors.
pub fn run(agent: &mut Agent) -> Result<(), Error> {
    crate::install_interrupt_handler()?;
    let mut terminal = Terminal::enter()?;
    let stdin = io::stdin();
    let mut events = EventReader::new(stdin.lock());
    let mut editor = Editor::default();
    let mut state = ViewState::from_agent(agent);
    terminal.draw(&mut state, &editor)?;

    loop {
        if agent.has_deferred_subagent_results() {
            state.activity = "delivering subagent results".into();
            state.turn_started = Some(std::time::Instant::now());
            terminal.draw(&mut state, &editor)?;
            match deferred_subagents_interactive(
                agent,
                &mut state,
                &mut editor,
                &mut terminal,
                &mut events,
            ) {
                Ok(Some(true) | None) => {}
                Ok(Some(false)) | Err(Error::Interrupted) => {
                    state.notice("Subagent result follow-up interrupted.")
                }
                Err(error) => state.notice(format!("Subagent result follow-up failed: {error}")),
            }
            rebuild_transcript_after_deferred_follow_up(&mut state, agent.messages());
            state.activity.clear();
            state.turn_started = None;
            terminal.draw(&mut state, &editor)?;
            continue;
        }
        if let Some(action) = state.pending_actions.pop_front() {
            activate_picker_action(agent, &mut state, action);
            terminal.draw(&mut state, &editor)?;
            continue;
        }
        if state.picker.is_none()
            && let Some(input) = state.queued_inputs.pop_front()
        {
            if handle_submission(
                agent,
                input,
                &mut state,
                &mut editor,
                &mut terminal,
                &mut events,
            )? {
                return Ok(());
            }
            continue;
        }

        let mut event = events.read_event()?;
        let mut needs_draw = false;
        loop {
            needs_draw |= !matches!(&event, Event::Tick);
            if matches!(&event, Event::Tick) && crate::interrupted() {
                crate::set_interrupted(false);
                if !processes::handle_interrupt(&mut state) {
                    state.subagent_manager.interrupt_all();
                    if !editor.is_empty() {
                        editor.clear();
                    }
                    state.activity = "input cleared".into();
                }
                needs_draw = true;
            }
            if state.subagent_view.is_some() {
                needs_draw = true;
                subagents::handle_event(&mut state, &mut editor, event);
                break;
            }
            if state.process_view.is_some() {
                needs_draw = true;
                processes::handle_event(&mut state, event);
                break;
            }
            if state.picker.is_some() {
                match event {
                    Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
                    Event::Key(key) => {
                        if let Some(action) = take_picker_action(&mut state, &mut editor, key) {
                            activate_picker_action(agent, &mut state, action);
                        }
                    }
                    Event::Paste(text) if picker_is_editing(&state) => editor.paste(&text),
                    Event::Mouse(mouse) => {
                        handle_mouse_selection(&mut terminal, &mut state, mouse)?
                    }
                    Event::Tick => {
                        needs_draw |= advance_ticks(&mut state);
                        needs_draw |= connection::poll(&mut state);
                    }
                    Event::MouseScroll(_) | Event::Paste(_) => {}
                }
                break;
            }
            match event {
                Event::Tick => {
                    needs_draw |= advance_ticks(&mut state);
                    needs_draw |= connection::poll(&mut state);
                }
                Event::MouseScroll(amount) => scroll(&mut state, amount),
                Event::Mouse(mouse) => handle_mouse_selection(&mut terminal, &mut state, mouse)?,
                Event::Paste(text) => {
                    if state.transcript.search_active() {
                        state.transcript.search_paste(&text);
                    } else {
                        state.transcript.blur();
                        if text.is_empty() {
                            paste_clipboard_image(agent, &mut state, &mut editor);
                        } else {
                            editor.paste(&text);
                        }
                        state.scroll_offset = 0;
                    }
                }
                Event::Key(key) => {
                    if navigation::handle_key(&mut state, &mut terminal, key)? {
                        // Transcript search, focus, or the block viewer owns the key.
                    } else {
                        match key {
                            Key::PageUp => scroll(&mut state, 10),
                            Key::PageDown => scroll(&mut state, -10),
                            Key::Ctrl('c') => {
                                editor.clear();
                                state.activity = "input cleared".into();
                            }
                            Key::Ctrl('l') => terminal.invalidate(),
                            Key::Ctrl('o') => toggle_tool_expansion(&mut state),
                            Key::Ctrl('v') | Key::Super('v') => {
                                paste_clipboard_image(agent, &mut state, &mut editor)
                            }
                            _ if handle_completion_key(&mut state, &mut editor, key) => {
                                // The completion menu consumed navigation or Tab.
                            }
                            Key::Tab => navigation::focus_transcript(&mut state),
                            _ => match editor.handle_key(key) {
                                EditAction::Submit(input) | EditAction::Steer(input) => {
                                    if handle_submission(
                                        agent,
                                        input,
                                        &mut state,
                                        &mut editor,
                                        &mut terminal,
                                        &mut events,
                                    )? {
                                        return Ok(());
                                    }
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
        if needs_draw || terminal.size_changed() {
            terminal.draw(&mut state, &editor)?;
        }
    }
}

fn rebuild_transcript_after_deferred_follow_up(
    state: &mut ViewState,
    messages: &[crate::provider::Message],
) {
    state.transcript = Transcript::from_messages(messages);
    state.render_cache.invalidate();
}

fn paste_clipboard_image(agent: &Agent, state: &mut ViewState, editor: &mut Editor) {
    let supported = crate::model::supports_images(agent.config(), agent.model());
    match editor.paste_clipboard_image(supported) {
        Ok(()) => {
            state.scroll_offset = 0;
        }
        Err(error) => state.notice(format!("Could not paste image: {error}.")),
    }
}

fn displayed_submission(editor: &Editor, input: &Submission) -> String {
    editor.expand_pastes(&input.text)
}

fn handle_submission<R: Read>(
    agent: &mut Agent,
    input: Submission,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<bool, Error> {
    let command = input.text.trim().to_string();
    if input.has_images() && !crate::model::supports_images(agent.config(), agent.model()) {
        state.notice(format!(
            "Model '{}' does not accept image input.",
            agent.model()
        ));
        editor.restore_submission(input);
        return Ok(false);
    }
    if let Some(skill_command) = command.strip_prefix("/skill:") {
        let (name, arguments) = skill_command
            .split_once(char::is_whitespace)
            .map_or((skill_command, ""), |(name, arguments)| {
                (name, arguments.trim())
            });
        let skills = crate::skills::scan(agent.config());
        if let Some(skill) = skills.iter().find(|skill| skill.name == name) {
            let expanded = crate::skills::expand(skill, &editor.expand_submission(arguments));
            let agent_input = match input.turn_input(expanded) {
                Ok(input) => input,
                Err(error) => {
                    state.notice(format!("Could not prepare images: {error}."));
                    editor.restore_submission(input);
                    return Ok(false);
                }
            };
            run_agent_turn(
                agent,
                Some(displayed_submission(editor, &input)),
                TurnDispatch::Normal(Some(agent_input)),
                state,
                editor,
                terminal,
                events,
            )?;
        } else {
            state.notice(format!(
                "Unknown skill '{name}'. Type /skills to list skills."
            ));
            editor.restore_submission(input);
        }
        return Ok(false);
    }
    if let Some(command) = command.strip_prefix('/') {
        if input.has_images() {
            state.notice("Images can accompany prompts and /skill commands, not local commands.");
            editor.restore_submission(input);
            return Ok(false);
        }
        let (name, argument) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, argument)| (name, argument.trim()));
        match name {
            "quit" | "q" => return Ok(true),
            "help" => state.notice(HELP),
            "model" if argument.is_empty() => open_model_picker(agent, state, false),
            "model" => {
                agent.switch_model(argument.to_string());
                state.model = agent.model().to_string();
                state.context_window = agent.context_window();
                state.context_tokens = 0;
                if crate::model::is_codex(agent.config(), agent.model()) {
                    open_reasoning_picker(agent, state, false);
                } else {
                    state.notice(format!("Switched to {}.", agent.model()));
                }
            }
            "settings" if argument.is_empty() => open_settings_picker(agent, state),
            "settings" => {
                let _ = settings(agent, argument, state);
                state.refresh_completions(agent);
            }
            "connect" if argument.is_empty() => connection::open(state, agent.config(), false),
            "connect" => state.notice("Usage: /connect"),
            name if is_new_session_command(name) => match agent.reset() {
                Ok(()) => {
                    let queued_inputs = std::mem::take(&mut state.queued_inputs);
                    let pending_steers = std::mem::take(&mut state.pending_steers);
                    let pending_actions = std::mem::take(&mut state.pending_actions);
                    *state = ViewState::from_agent(agent);
                    state.queued_inputs = queued_inputs;
                    state.pending_steers = pending_steers;
                    state.pending_actions = pending_actions;
                }
                Err(error) => state.notice(format!("Could not start a session: {error}")),
            },
            "compact" => {
                state.apply(Update::Compacting);
                terminal.draw(state, editor)?;
                let result = compact_interactive(agent, state, editor, terminal, events);
                state.activity.clear();
                match result {
                    Ok(()) => {}
                    Err(Error::Interrupted) => state.notice("Compaction interrupted."),
                    Err(error) => state.notice(format!("Compaction failed: {error}")),
                }
            }
            "usage" if argument.is_empty() => commands::show_usage(state),
            "usage" => state.notice("Usage: /usage"),
            "tools" => {
                let registry = agent.scan_tools();
                let mut text = String::from("Available tools\n\n");
                for (tool, description, origin) in registry.describe_all() {
                    text.push_str(&format!("- `{tool}` ({origin}): {description}\n"));
                }
                for warning in registry.warnings {
                    text.push_str(&format!("\nWarning: {warning}"));
                }
                state.notice(text);
            }
            "skills" => show_skills(agent, state),
            "subagents" if argument.is_empty() => open_subagent_dashboard(state),
            "subagents" => state.notice("Usage: /subagents"),
            "ps" if argument.is_empty() => {
                processes::open_dashboard(state, agent.background_processes())
            }
            "ps" => state.notice("Usage: /ps"),
            "resume" if argument.is_empty() => open_resume_picker(agent, state),
            "resume" => resume(agent, argument, state),
            "unqueue" => unqueue(argument, state),
            "goal" => {
                let (agent_argument, displayed) = prepare_goal_submission(editor, argument);
                match goal_command(agent, &agent_argument, displayed, state) {
                    GoalDispatch::Idle => {}
                    GoalDispatch::Start(displayed) => {
                        run_agent_turn(
                            agent,
                            Some(displayed),
                            TurnDispatch::Goal,
                            state,
                            editor,
                            terminal,
                            events,
                        )?;
                    }
                    GoalDispatch::Resume => {
                        run_agent_turn(
                            agent,
                            None,
                            TurnDispatch::Goal,
                            state,
                            editor,
                            terminal,
                            events,
                        )?;
                    }
                }
            }
            "undo" => match agent.undo_last_turn() {
                Ok(report) => {
                    state.transcript = Transcript::from_messages(agent.messages());
                    state.render_cache.invalidate();
                    state.context_tokens = agent.context_tokens();
                    state.active_goal = agent.active_goal().map(str::to_string);
                    notice_undo(state, report);
                }
                Err(error) => state.notice(format!("Could not undo: {error}")),
            },
            "copy" => copy_last_reply(terminal, state, agent.messages())?,
            "copy-all" => copy_all_messages(terminal, state, agent.messages())?,
            "" => {}
            other => state.notice(format!("Unknown command '/{other}'. Type /help.")),
        }
        return Ok(false);
    }

    let displayed_input = displayed_submission(editor, &input);
    let agent_text = editor.expand_submission(&input.text);
    let agent_input = match input.turn_input(agent_text) {
        Ok(input) => input,
        Err(error) => {
            state.notice(format!("Could not prepare images: {error}."));
            editor.restore_submission(input);
            return Ok(false);
        }
    };
    run_agent_turn(
        agent,
        Some(displayed_input),
        TurnDispatch::Normal(Some(agent_input)),
        state,
        editor,
        terminal,
        events,
    )?;
    Ok(false)
}

enum GoalDispatch {
    Idle,
    Start(String),
    Resume,
}

enum TurnDispatch {
    Normal(Option<crate::provider::TurnInput>),
    Goal,
}

fn prepare_goal_submission(editor: &Editor, argument: &str) -> (String, String) {
    (
        editor.expand_submission(argument),
        editor.expand_pastes(argument),
    )
}

fn goal_command(
    agent: &mut Agent,
    argument: &str,
    displayed: String,
    state: &mut ViewState,
) -> GoalDispatch {
    match goal(agent, argument, state) {
        GoalAction::None => GoalDispatch::Idle,
        GoalAction::Start => GoalDispatch::Start(displayed),
        GoalAction::Resume => GoalDispatch::Resume,
    }
}

fn run_agent_turn<R: Read>(
    agent: &mut Agent,
    displayed_input: Option<String>,
    turn: TurnDispatch,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<(), Error> {
    let goal_mode = matches!(&turn, TurnDispatch::Goal);
    if let Some(displayed_input) = displayed_input {
        state.transcript.push_user(displayed_input);
    }
    state.activity = "sending".into();
    state.turn_started = Some(std::time::Instant::now());
    state.active_goal = agent.active_goal().map(str::to_string);
    state.goal_running = goal_mode;
    state.scroll_offset = 0;
    terminal.draw(state, editor)?;
    let agent_input = match turn {
        TurnDispatch::Normal(input) => input,
        TurnDispatch::Goal => None,
    };
    match turn_interactive(
        agent,
        agent_input,
        goal_mode,
        state,
        editor,
        terminal,
        events,
    ) {
        Ok(true) => {}
        Ok(false) | Err(Error::Interrupted) => state.notice("Turn interrupted."),
        Err(error) => state.notice(format!("Request failed: {error}")),
    }
    crate::set_interrupted(false);
    state.activity.clear();
    state.turn_started = None;
    state.active_goal = agent.active_goal().map(str::to_string);
    state.goal_running = false;
    Ok(())
}
