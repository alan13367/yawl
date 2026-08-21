//! Full-screen terminal UI built directly on termios and ANSI escape
//! sequences. The terminal remains responsive while the blocking agent loop
//! runs on a scoped worker thread.

mod commands;
#[cfg(test)]
mod commands_tests;
mod completion;
#[cfg(test)]
mod completion_tests;
pub mod events;
pub mod highlight;
pub mod input;
pub mod markdown;
mod picker;
#[cfg(test)]
mod picker_tests;
mod render;
#[cfg(test)]
mod render_tests;
mod state;
#[cfg(test)]
mod state_tests;
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
    HELP, activate_picker_action, is_new_session_command, open_resume_picker, resume, settings,
    show_skills, unqueue,
};
use self::completion::handle_completion_key;
use self::events::{Event, EventReader, Key};
use self::input::{EditAction, Editor};
use self::picker::{
    open_model_picker, open_reasoning_picker, open_settings_picker, picker_is_editing,
    take_picker_action,
};
use self::state::{ViewState, advance_ticks, scroll, toggle_tool_expansion};
use self::terminal::Terminal;
use self::worker::{compact_interactive, handle_mouse_selection, turn_interactive};

#[cfg(test)]
use self::commands::{handle_queue_picker_action, open_queue_picker};
#[cfg(test)]
use self::completion::Completion;
#[cfg(test)]
use self::picker::{ActivePickers, Picker, PickerAction, PickerItem, color_picker, render_picker};
#[cfg(test)]
use self::render::{build_frame, render_entries, render_loading_state, render_queued_panel};
#[cfg(test)]
use self::state::Update;
#[cfg(test)]
use self::terminal::{
    ScreenPoint, TextSelection, base64_encode, highlighted_selection, selected_text,
};
#[cfg(test)]
use self::transcript::{Entry, Transcript, TranscriptEvent};
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
    if state.transcript.is_empty() {
        state.notice("Yawl is ready. Type /help for commands.");
    }
    terminal.draw(&mut state, &editor)?;

    loop {
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

        let event = events.read_event()?;
        if state.picker.is_some() {
            match event {
                Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
                Event::Key(key) => {
                    if let Some(action) = take_picker_action(&mut state, &mut editor, key) {
                        activate_picker_action(agent, &mut state, action);
                    }
                }
                Event::Paste(text) if picker_is_editing(&state) => editor.paste(&text),
                Event::Mouse(mouse) => handle_mouse_selection(&mut terminal, &mut state, mouse)?,
                Event::Tick => advance_ticks(&mut state),
                Event::MouseScroll(_) | Event::Paste(_) => {}
            }
            terminal.draw(&mut state, &editor)?;
            continue;
        }
        match event {
            Event::Tick => {
                advance_ticks(&mut state);
                if crate::interrupted() {
                    crate::set_interrupted(false);
                    if !editor.is_empty() {
                        editor.clear();
                    }
                    state.activity = "input cleared".into();
                }
            }
            Event::MouseScroll(amount) => scroll(&mut state, amount),
            Event::Mouse(mouse) => handle_mouse_selection(&mut terminal, &mut state, mouse)?,
            Event::Paste(text) => {
                editor.paste(&text);
                state.scroll_offset = 0;
            }
            Event::Key(Key::PageUp) => scroll(&mut state, 10),
            Event::Key(Key::PageDown) => scroll(&mut state, -10),
            Event::Key(Key::Ctrl('c')) => {
                editor.clear();
                state.activity = "input cleared".into();
            }
            Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
            Event::Key(Key::Ctrl('o')) => toggle_tool_expansion(&mut state),
            Event::Key(key) => {
                if handle_completion_key(&mut state, &mut editor, key) {
                    // The completion menu consumed navigation or Tab.
                } else if let EditAction::Submit(input) = editor.handle_key(key)
                    && handle_submission(
                        agent,
                        input,
                        &mut state,
                        &mut editor,
                        &mut terminal,
                        &mut events,
                    )?
                {
                    return Ok(());
                }
            }
        }
        terminal.draw(&mut state, &editor)?;
    }
}

fn handle_submission<R: Read>(
    agent: &mut Agent,
    input: String,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<bool, Error> {
    let command = input.trim();
    if let Some(skill_command) = command.strip_prefix("/skill:") {
        let (name, arguments) = skill_command
            .split_once(char::is_whitespace)
            .map_or((skill_command, ""), |(name, arguments)| {
                (name, arguments.trim())
            });
        let skills = crate::skills::scan(agent.config());
        if let Some(skill) = skills.iter().find(|skill| skill.name == name) {
            let expanded = crate::skills::expand(skill, arguments);
            run_agent_submission(agent, input, expanded, state, editor, terminal, events)?;
        } else {
            state.notice(format!(
                "Unknown skill '{name}'. Type /skills to list skills."
            ));
        }
        return Ok(false);
    }
    if let Some(command) = command.strip_prefix('/') {
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
            name if is_new_session_command(name) => match agent.reset() {
                Ok(()) => {
                    let queued_inputs = std::mem::take(&mut state.queued_inputs);
                    let pending_actions = std::mem::take(&mut state.pending_actions);
                    *state = ViewState::from_agent(agent);
                    state.queued_inputs = queued_inputs;
                    state.pending_actions = pending_actions;
                    state.notice("Started a new session.");
                }
                Err(error) => state.notice(format!("Could not start a session: {error}")),
            },
            "compact" => {
                terminal.draw(state, editor)?;
                match compact_interactive(agent, state, editor, terminal, events) {
                    Ok(()) => {}
                    Err(Error::Interrupted) => state.notice("Compaction interrupted."),
                    Err(error) => state.notice(format!("Compaction failed: {error}")),
                }
            }
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
            "resume" if argument.is_empty() => open_resume_picker(agent, state),
            "resume" => resume(agent, argument, state),
            "unqueue" => unqueue(argument, state),
            "" => {}
            other => state.notice(format!("Unknown command '/{other}'. Type /help.")),
        }
        return Ok(false);
    }

    run_agent_submission(agent, input.clone(), input, state, editor, terminal, events)?;
    Ok(false)
}

fn run_agent_submission<R: Read>(
    agent: &mut Agent,
    displayed_input: String,
    agent_input: String,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<(), Error> {
    state.transcript.push_user(displayed_input);
    state.activity = "sending".into();
    state.scroll_offset = 0;
    terminal.draw(state, editor)?;
    match turn_interactive(agent, agent_input, state, editor, terminal, events) {
        Ok(true) => {}
        Ok(false) | Err(Error::Interrupted) => state.notice("Turn interrupted."),
        Err(error) => state.notice(format!("Request failed: {error}")),
    }
    crate::set_interrupted(false);
    state.activity.clear();
    Ok(())
}
