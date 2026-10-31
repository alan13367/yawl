//! Full-screen terminal UI built directly on a raw terminal and ANSI escape
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
mod controller;
mod dashboard;
pub mod events;
mod files;
pub mod git;
pub mod highlight;
pub mod input;
pub mod markdown;
mod navigation;
mod picker;
#[cfg(test)]
mod picker_tests;
mod processes;
mod remote;
mod render;
#[cfg(test)]
mod render_tests;
mod search;
mod setup;
mod state;
#[cfg(test)]
mod state_tests;
mod status_bar;
mod subagents;
mod terminal;
#[cfg(test)]
mod terminal_tests;
#[cfg(test)]
mod test_support;
mod theme;
mod tool_view;
mod transcript;
mod worker;
#[cfg(test)]
mod worker_tests;

use crate::agent::{Agent, TurnRequest};
use crate::error::Error;

pub(crate) use self::setup::{TrustAnswer, ask_project_trust, run_setup};

use self::commands::{
    Command, GoalAction, HOTKEYS, PlanAction, copy_all_messages, copy_last_reply, goal, help,
    notice_undo, open_resume_picker, parse_command, plan, reasoning, resume, revision_text,
    settings, show_diff, show_skills, unqueue,
};
use self::controller::Job;
use self::events::EventReader;
use self::input::{Editor, Submission};
use self::picker::{open_model_picker, open_settings_picker};
use self::state::ViewState;
use self::subagents::open_dashboard as open_subagent_dashboard;
use self::terminal::Terminal;
use self::transcript::Transcript;

#[cfg(test)]
use self::commands::{activate_picker_action, busy_command};
#[cfg(test)]
use self::commands::{
    format_copy_all, handle_queue_picker_action, last_assistant_reply, open_queue_picker,
};
#[cfg(test)]
use self::completion::handle_completion_key;
#[cfg(test)]
use self::completion::{
    COMPLETION_MENU_ROWS, Completion, completion_window, matching_completions,
    sync_completion_filter,
};
#[cfg(test)]
use self::events::Key;
#[cfg(test)]
use self::input::EditAction;
#[cfg(test)]
use self::picker::{
    ActivePickers, Picker, PickerAction, PickerItem, color_picker, render_picker,
    selection_color_picker,
};
#[cfg(test)]
use self::picker::{picker_is_editing, sync_color_preview, take_picker_action};
#[cfg(test)]
use self::render::{
    HIDDEN_CURSOR, RenderCache, WELCOME_ANIMATION_TICKS, build_frame, render_entries,
    render_loading_state, render_queued_panel, selected_row, selection_style,
};
#[cfg(test)]
use self::state::{Update, advance_ticks};
#[cfg(test)]
use self::terminal::{
    ScreenPoint, TextSelection, cursor_control, highlighted_selection, selected_text,
};
#[cfg(test)]
use self::transcript::{Entry, TranscriptEvent};
#[cfg(test)]
use self::worker::{activate_picker_action_while_busy, is_cancel_key};
#[cfg(test)]
use crate::config::{Config, UiColor};
#[cfg(test)]
use crate::provider::ReasoningKind;

/// Runs the alternate-screen terminal interface until `/quit`.
///
/// # Errors
///
/// Returns terminal setup, rendering, session, or input errors.
pub fn run(agent: &mut Agent) -> Result<(), Error> {
    crate::install_interrupt_handler()?;
    agent.enable_interactive_questions();
    let mut terminal = Terminal::enter()?;
    // An explicit theme needs no answer from the terminal, which some
    // multiplexers and slow links never give in time.
    let (background, typed) = if agent.config().theme == crate::config::Theme::Auto {
        terminal.query_background()
    } else {
        (None, Vec::new())
    };
    theme::init(agent.config().theme, background);
    let events = EventReader::with_pending(
        terminal
            .remote()
            .terminal_input(remote::TerminalInput::stdin()),
        &typed,
    );
    let editor = Editor::default();
    let mut state = ViewState::from_agent(agent);
    if let Some(warning) = agent.session_warning() {
        state.notice(warning);
    }
    controller::run(agent, state, editor, terminal, events)
}

/// The job that re-drives a turn that stopped through an error or
/// interrupt, or `None` with a notice when the last turn finished.
fn continue_turn(agent: &Agent, state: &mut ViewState) -> Option<Job> {
    if !agent.can_continue() {
        state.continue_offered = false;
        state.notice("Nothing to continue: the last turn finished.");
        return None;
    }
    Some(Job::Turn {
        displayed: None,
        request: TurnRequest::Continue,
    })
}

/// Announces a settled turn through the terminal bell when the user may be
/// away. A queued message suppresses the ring because another turn starts
/// immediately.
fn notify_settled(terminal: &mut Terminal, state: &ViewState) {
    if state.bell && state.queued_inputs.is_empty() {
        terminal.ring_bell();
    }
}

fn displayed_submission(editor: &Editor, input: &Submission) -> String {
    editor.expand_pastes(&input.text)
}

/// What an idle submission asks the event loop to do next.
enum Submitted {
    Handled,
    Quit,
    Start(Job),
}

fn turn(displayed: Option<String>, request: TurnRequest) -> Submitted {
    Submitted::Start(Job::Turn { displayed, request })
}

fn handle_submission(
    agent: &mut Agent,
    input: Submission,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
) -> Result<Submitted, Error> {
    let command = input.text.trim().to_string();
    if input.has_images() && !crate::model::supports_images(agent.config(), agent.model()) {
        state.notice(format!(
            "Model '{}' does not accept image input.",
            agent.model()
        ));
        editor.restore_submission(input);
        return Ok(Submitted::Handled);
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
                    return Ok(Submitted::Handled);
                }
            };
            return Ok(turn(
                Some(displayed_submission(editor, &input)),
                TurnRequest::Prompt(Some(agent_input)),
            ));
        } else {
            state.notice(format!(
                "Unknown skill '{name}'. Type /skills to list skills."
            ));
            editor.restore_submission(input);
        }
        return Ok(Submitted::Handled);
    }
    if let Some(typed) = parse_command(&command) {
        if input.has_images() {
            state.notice("Images can accompany prompts and /skill commands, not local commands.");
            editor.restore_submission(input);
            return Ok(Submitted::Handled);
        }
        let argument = typed.argument;
        let Some(command) = typed.command else {
            if !typed.name.is_empty() {
                state.notice(format!("Unknown command '/{}'. Type /help.", typed.name));
            }
            return Ok(Submitted::Handled);
        };
        match command {
            Command::Quit => return Ok(Submitted::Quit),
            Command::Help => state.notice(help()),
            Command::Hotkeys => state.notice(HOTKEYS),
            Command::Diff => show_diff(agent, state),
            Command::Continue => {
                if let Some(job) = continue_turn(agent, state) {
                    return Ok(Submitted::Start(job));
                }
            }
            Command::Init => match commands::init(argument) {
                Ok(input) => return Ok(turn(Some("/init".into()), TurnRequest::Init(input))),
                Err(usage) => state.notice(usage),
            },
            Command::Model if argument.is_empty() => open_model_picker(agent, state, false),
            Command::Model => commands::switch_model(agent, argument.to_string(), state),
            Command::Reasoning => reasoning(agent, argument, state),
            Command::Settings if argument.is_empty() => open_settings_picker(agent, state),
            Command::Settings => {
                let _ = settings(agent, argument, state);
                state.refresh_completions(agent);
            }
            Command::Connect if argument.is_empty() => {
                connection::open(state, agent.config(), false)
            }
            Command::Connect => state.notice("Usage: /connect"),
            Command::New => commands::new_session(agent, state),
            Command::Compact => return Ok(Submitted::Start(Job::Compact)),
            Command::Usage if argument.is_empty() => commands::show_usage(state),
            Command::Usage => state.notice("Usage: /usage"),
            Command::Tools => commands::show_tools(agent, state),
            Command::Skills => show_skills(agent, state),
            Command::Subagents if argument.is_empty() => open_subagent_dashboard(state),
            Command::Subagents => state.notice("Usage: /subagents"),
            Command::Processes if argument.is_empty() => {
                processes::open_dashboard(state, agent.background_processes())
            }
            Command::Processes => state.notice("Usage: /ps"),
            Command::Git if argument.is_empty() => git::open_dashboard(state),
            Command::Git => state.notice("Usage: /git"),
            Command::Remote => remote::command(terminal, state, argument),
            Command::Resume if argument.is_empty() => open_resume_picker(agent, state),
            Command::Resume => resume(agent, argument, state),
            Command::Unqueue => unqueue(argument, state),
            Command::Goal => {
                let (agent_argument, displayed) = prepare_goal_submission(editor, argument);
                match goal(agent, &agent_argument, state) {
                    GoalAction::None => {}
                    GoalAction::Start => return Ok(turn(Some(displayed), TurnRequest::Goal)),
                    GoalAction::Resume => return Ok(turn(None, TurnRequest::Goal)),
                }
            }
            Command::Plan => {
                let (agent_argument, displayed) = prepare_goal_submission(editor, argument);
                match plan(agent, &agent_argument, state) {
                    PlanAction::None => {}
                    PlanAction::Start => return Ok(turn(Some(displayed), TurnRequest::Plan)),
                    PlanAction::Resume => return Ok(turn(None, TurnRequest::Plan)),
                    PlanAction::Implement => {
                        return Ok(turn(None, TurnRequest::PlanImplement(None)));
                    }
                    PlanAction::Revise(revision) => {
                        let displayed = revision_text(&displayed).unwrap_or(&revision);
                        return Ok(turn(
                            Some(displayed.to_string()),
                            TurnRequest::PlanRevise(revision.into()),
                        ));
                    }
                }
            }
            Command::Undo => match agent.undo_last_turn() {
                Ok(report) => {
                    state.transcript = Transcript::from_messages(agent.messages());
                    state.refresh_welcome(agent);
                    state.render_cache.invalidate();
                    state.context_tokens = agent.context_tokens();
                    state.sync_modes(agent);
                    notice_undo(state, report);
                }
                Err(error) => state.notice(format!("Could not undo: {error}")),
            },
            Command::Copy => copy_last_reply(terminal, state, agent.messages())?,
            Command::CopyAll => copy_all_messages(terminal, state, agent.messages())?,
        }
        return Ok(Submitted::Handled);
    }

    let displayed_input = displayed_submission(editor, &input);
    let agent_text = editor.expand_submission(&input.text);
    let agent_input = match input.turn_input(agent_text) {
        Ok(input) => input,
        Err(error) => {
            state.notice(format!("Could not prepare images: {error}."));
            editor.restore_submission(input);
            return Ok(Submitted::Handled);
        }
    };
    let request = if agent.active_plan().is_some() {
        TurnRequest::PlanFollowUp(agent_input)
    } else {
        TurnRequest::Prompt(Some(agent_input))
    };
    Ok(turn(Some(displayed_input), request))
}

fn prepare_goal_submission(editor: &Editor, argument: &str) -> (String, String) {
    (
        editor.expand_submission(argument),
        editor.expand_pastes(argument),
    )
}
