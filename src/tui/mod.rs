//! Full-screen terminal UI built directly on termios and ANSI escape
//! sequences. The terminal remains responsive while the blocking agent loop
//! runs on a scoped worker thread.

pub mod events;
pub mod highlight;
pub mod input;
pub mod markdown;
mod tool_view;
mod transcript;

use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::{Agent, TurnEvent};
use crate::config::{ConfigChange, ConfigChangeEffect, SkillDirectoryAction, UiColor};
use crate::error::Error;
use crate::provider::ReasoningKind;

use self::events::{Event, EventReader, Key, MouseEvent, MouseKind};
use self::input::{EditAction, Editor};
use self::transcript::{Entry, Transcript, TranscriptEvent};

const USER_BACKGROUND: &str = "\x1b[48;2;52;53;64m";
const USER_TEXT: &str = "\x1b[38;2;208;208;214m";
const SETTINGS_ACCENT_COLOR_INDEX: usize = 4;
const SETTINGS_AUTO_COMPACT_INDEX: usize = 5;
const SETTINGS_RELOAD_INDEX: usize = 12;
const COPY_TOAST_TICKS: u8 = 15;

const HELP: &str = "\
Commands
  /model [MODEL]       open the model picker or switch directly
  /settings [KEY ...]  open the settings picker or change directly
  /new                 start a new session without changing directories
  /clear               alias for /new
  /compact             summarize older messages now
  /tools               list builtin and discovered tools
  /skills              list discovered skills and search directories
  /skill:NAME [ARGS]   run a discovered skill
  /resume [ID|NUMBER]  open the session picker or resume directly
  /unqueue [N|all]     cancel queued messages
  /help                show this help
  /quit                leave Yawl

Input
  Enter submits. Shift+Enter or Alt+Enter inserts a newline.
  Type / for commands; Up/Down select, Tab completes, and Enter accepts a sole match.
  Model and settings pickers remain available during an active response.
  Messages submitted during a response appear below it as queued.
  Outside the menu, Up and Down browse input history. Ctrl+U, Ctrl+K, and Ctrl+W edit.
  Ctrl+O expands or collapses tool output. Esc or Ctrl+C aborts the active turn.
  Mouse wheel and PageUp/PageDown scroll. Drag selects text; release copies it.
";

#[derive(Clone)]
struct Completion {
    command: String,
    description: String,
}

#[derive(Clone)]
enum PickerAction {
    SwitchModel(String),
    SaveModel(String),
    OpenModels { save: bool },
    OpenReasoning { save: bool },
    SetReasoning { effort: Option<String>, save: bool },
    SetHideReasoning(bool),
    OpenAccentColor,
    SetAccentColor(UiColor),
    ResumeSession(String),
    EditSetting { key: String, initial: String },
    EditModel { save: bool, initial: String },
    ApplySetting { argument: String, selected: usize },
    SetAutoCompact(bool),
    RemoveQueued(usize),
    ClearQueued,
    Reload,
    ShowSettings,
}

#[derive(Clone)]
struct PickerItem {
    label: String,
    description: String,
    action: PickerAction,
}

#[derive(Clone)]
struct Picker {
    title: String,
    hint: String,
    items: Vec<PickerItem>,
    selected: usize,
    editing: Option<PickerEdit>,
}

#[derive(Clone)]
enum PickerEdit {
    Setting(String),
    Model { save: bool },
}

struct ActivePickers {
    model: Picker,
    default_model: Picker,
    settings: Picker,
    reasoning: Picker,
    default_reasoning: Picker,
    accent_color: Picker,
}

impl ActivePickers {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            model: model_picker(agent, false),
            default_model: model_picker(agent, true),
            settings: settings_picker(agent),
            reasoning: reasoning_picker(agent, false),
            default_reasoning: reasoning_picker(agent, true),
            accent_color: color_picker(agent.config().accent_color),
        }
    }
}

struct ViewState {
    transcript: Transcript,
    tools_expanded: bool,
    model: String,
    reasoning_effort: Option<String>,
    hide_reasoning: bool,
    accent_color: UiColor,
    copy_toast_ticks: u8,
    context_tokens: u64,
    context_window: u64,
    activity: String,
    scroll_offset: usize,
    queued_inputs: std::collections::VecDeque<String>,
    pending_actions: std::collections::VecDeque<PickerAction>,
    completions: Vec<Completion>,
    completion_index: usize,
    picker: Option<Picker>,
}

impl ViewState {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            transcript: Transcript::from_messages(agent.messages()),
            tools_expanded: false,
            model: agent.model().to_string(),
            reasoning_effort: agent.config().reasoning_effort.clone(),
            hide_reasoning: agent.config().hide_reasoning,
            accent_color: agent.config().accent_color,
            copy_toast_ticks: 0,
            context_tokens: agent.context_tokens(),
            context_window: agent.context_window(),
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            pending_actions: std::collections::VecDeque::new(),
            completions: command_completions(agent),
            completion_index: 0,
            picker: None,
        }
    }

    fn refresh_completions(&mut self, agent: &Agent) {
        self.completions = command_completions(agent);
        self.completion_index = 0;
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.transcript.notice(text.into());
        self.scroll_offset = 0;
    }

    fn apply(&mut self, update: Update) {
        let follow_bottom = self.scroll_offset == 0;
        match update {
            Update::Transcript(event) => {
                self.activity = match &event {
                    TranscriptEvent::TextDelta(_) => "responding".into(),
                    TranscriptEvent::ReasoningDelta { .. } if !self.hide_reasoning => {
                        "reasoning".into()
                    }
                    TranscriptEvent::ReasoningDelta { .. } => "responding".into(),
                    TranscriptEvent::ToolStart { .. } => "running tool".into(),
                    TranscriptEvent::AssistantDone | TranscriptEvent::ToolEnd { .. } => {
                        String::new()
                    }
                    TranscriptEvent::RetryReset => self.activity.clone(),
                };
                self.transcript.apply(event);
            }
            Update::Retrying {
                attempt,
                delay_ms,
                error,
            } => {
                self.activity = format!(
                    "attempt {attempt} failed, retrying in {delay_ms}ms: {}",
                    crate::error::truncate(&error, 80)
                );
            }
            Update::Compacting => self.activity = "compacting conversation".into(),
            Update::Compacted { replaced } => {
                self.activity.clear();
                self.notice(format!("Compacted {replaced} older messages."));
            }
            Update::Usage {
                context_tokens,
                context_window,
            } => {
                self.context_tokens = context_tokens;
                self.context_window = context_window;
            }
        }
        if follow_bottom {
            self.scroll_offset = 0;
        }
    }
}

enum Update {
    Transcript(TranscriptEvent),
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    Compacting,
    Compacted {
        replaced: usize,
    },
    Usage {
        context_tokens: u64,
        context_window: u64,
    },
}

impl Update {
    fn from_event(event: TurnEvent<'_>) -> Self {
        match event {
            TurnEvent::TextDelta(text) => {
                Self::Transcript(TranscriptEvent::TextDelta(text.to_string()))
            }
            TurnEvent::ReasoningDelta { kind, text } => {
                Self::Transcript(TranscriptEvent::ReasoningDelta {
                    kind,
                    text: text.to_string(),
                })
            }
            TurnEvent::RetryReset => Self::Transcript(TranscriptEvent::RetryReset),
            TurnEvent::Retrying {
                attempt,
                delay_ms,
                error,
            } => Self::Retrying {
                attempt,
                delay_ms,
                error,
            },
            TurnEvent::AssistantDone => Self::Transcript(TranscriptEvent::AssistantDone),
            TurnEvent::ToolStart { name, args } => Self::Transcript(TranscriptEvent::ToolStart {
                name: name.to_string(),
                args: args.to_string(),
            }),
            TurnEvent::ToolEnd {
                name,
                output,
                is_error,
            } => Self::Transcript(TranscriptEvent::ToolEnd {
                name: name.to_string(),
                output: output.to_string(),
                is_error,
            }),
            TurnEvent::Compacting => Self::Compacting,
            TurnEvent::Compacted { replaced } => Self::Compacted { replaced },
            TurnEvent::Usage {
                context_tokens,
                context_window,
            } => Self::Usage {
                context_tokens,
                context_window,
            },
        }
    }
}

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
                Event::Tick => advance_copy_toast(&mut state),
                Event::MouseScroll(_) | Event::Paste(_) => {}
            }
            terminal.draw(&mut state, &editor)?;
            continue;
        }
        match event {
            Event::Tick => {
                advance_copy_toast(&mut state);
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

fn is_new_session_command(name: &str) -> bool {
    matches!(name, "new" | "clear")
}

fn command_completions(agent: &Agent) -> Vec<Completion> {
    let mut completions = [
        ("/model", "List or switch models"),
        ("/settings", "Show or change settings"),
        ("/new", "Start a session without changing directories"),
        ("/clear", "Alias for /new"),
        ("/compact", "Summarize older messages"),
        ("/tools", "List available tools"),
        ("/skills", "List available skills"),
        ("/resume", "List or resume sessions"),
        ("/unqueue", "Cancel queued messages"),
        ("/help", "Show help"),
        ("/quit", "Exit Yawl"),
    ]
    .into_iter()
    .map(|(command, description)| Completion {
        command: command.into(),
        description: description.into(),
    })
    .collect::<Vec<_>>();
    completions.extend(
        crate::skills::scan(agent.config())
            .into_iter()
            .map(|skill| Completion {
                command: format!("/skill:{}", skill.name),
                description: skill.description,
            }),
    );
    completions
}

fn matching_completions<'a>(state: &'a ViewState, editor: &Editor) -> Vec<&'a Completion> {
    let Some(prefix) = editor.command_prefix() else {
        return Vec::new();
    };
    state
        .completions
        .iter()
        .filter(|completion| completion.command.starts_with(&prefix))
        .take(8)
        .collect()
}

fn handle_completion_key(state: &mut ViewState, editor: &mut Editor, key: Key) -> bool {
    let matches = matching_completions(state, editor)
        .into_iter()
        .map(|completion| completion.command.clone())
        .collect::<Vec<_>>();
    if matches.is_empty() {
        state.completion_index = 0;
        return false;
    }
    state.completion_index = state.completion_index.min(matches.len() - 1);
    match key {
        Key::Up => {
            state.completion_index = state.completion_index.saturating_sub(1);
            true
        }
        Key::Down => {
            state.completion_index = (state.completion_index + 1).min(matches.len() - 1);
            true
        }
        Key::Tab => {
            editor.complete_command(&matches[state.completion_index]);
            state.completion_index = 0;
            true
        }
        Key::Enter if matches.len() == 1 => {
            editor.complete_command(&matches[0]);
            state.completion_index = 0;
            false
        }
        _ => false,
    }
}

fn show_skills(agent: &Agent, state: &mut ViewState) {
    let skills = crate::skills::scan(agent.config());
    let mut text = String::from("Skill directories\n\n");
    for dir in &agent.config().skill_dirs {
        text.push_str(&format!("- `{}`\n", dir.display()));
    }
    if skills.is_empty() {
        text.push_str("\nNo skills found. Add one with `/settings skills add DIRECTORY`.");
    } else {
        text.push_str("\nAvailable skills\n\n");
        for skill in skills {
            text.push_str(&format!(
                "- `/skill:{}`: {}\n",
                skill.name, skill.description
            ));
        }
    }
    state.notice(text);
}

fn open_model_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    state.picker = Some(model_picker(agent, save));
}

fn model_picker(agent: &Agent, save: bool) -> Picker {
    let selected_model = if save {
        agent.config().model.as_deref().unwrap_or(agent.model())
    } else {
        agent.model()
    };
    let mut models = crate::model::available_models(agent.config());
    if !models.iter().any(|(model, _)| model == selected_model) {
        models.push((
            selected_model.to_string(),
            if save {
                "Current default"
            } else {
                "Current model"
            }
            .into(),
        ));
        models.sort_by(|left, right| left.0.cmp(&right.0));
    }
    let mut items = models
        .into_iter()
        .map(|(model, name)| PickerItem {
            label: name,
            description: model.clone(),
            action: if save {
                PickerAction::SaveModel(model)
            } else {
                PickerAction::SwitchModel(model)
            },
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Use another model ID…".into(),
        description: "Enter a model not listed above".into(),
        action: PickerAction::EditModel {
            save,
            initial: String::new(),
        },
    });
    let selected = items
        .iter()
        .position(|item| item.description == selected_model)
        .unwrap_or(0);
    Picker {
        title: if save {
            "Default model".into()
        } else {
            "Choose model".into()
        },
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
    }
}

fn open_settings_picker(agent: &Agent, state: &mut ViewState) {
    state.picker = Some(settings_picker(agent));
}

fn settings_picker(agent: &Agent) -> Picker {
    let on_off = if agent.config().auto_compact {
        "On"
    } else {
        "Off"
    };
    let reasoning_visibility = if agent.config().hide_reasoning {
        "Hidden"
    } else {
        "Visible"
    };
    Picker {
        title: "Settings".into(),
        hint: "↑/↓ move  Enter change  Esc close".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Default model".into(),
                description: agent
                    .config()
                    .model
                    .clone()
                    .unwrap_or_else(|| agent.model().to_string()),
                action: PickerAction::OpenModels { save: true },
            },
            PickerItem {
                label: "Max output tokens".into(),
                description: agent.config().max_tokens.to_string(),
                action: PickerAction::EditSetting {
                    key: "max_tokens".into(),
                    initial: agent.config().max_tokens.to_string(),
                },
            },
            PickerItem {
                label: "Codex reasoning effort".into(),
                description: agent
                    .config()
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "provider default".into()),
                action: if crate::model::is_codex(agent.config(), agent.model()) {
                    PickerAction::OpenReasoning { save: true }
                } else {
                    PickerAction::EditSetting {
                        key: "reasoning_effort".into(),
                        initial: agent
                            .config()
                            .reasoning_effort
                            .clone()
                            .unwrap_or_else(|| "default".into()),
                    }
                },
            },
            PickerItem {
                label: "Reasoning display".into(),
                description: format!("{reasoning_visibility} · Enter to toggle"),
                action: PickerAction::SetHideReasoning(!agent.config().hide_reasoning),
            },
            PickerItem {
                label: "Accent color".into(),
                description: agent.config().accent_color.config_value(),
                action: PickerAction::OpenAccentColor,
            },
            PickerItem {
                label: "Automatic compaction".into(),
                description: format!("{on_off} · Enter to toggle"),
                action: PickerAction::SetAutoCompact(!agent.config().auto_compact),
            },
            PickerItem {
                label: "Compaction threshold".into(),
                description: format!("{:.0}%", agent.config().compact_threshold * 100.0),
                action: PickerAction::EditSetting {
                    key: "compact_threshold".into(),
                    initial: format!("{:.0}%", agent.config().compact_threshold * 100.0),
                },
            },
            PickerItem {
                label: "Current model context window".into(),
                description: agent.context_window().to_string(),
                action: PickerAction::EditSetting {
                    key: "context_window".into(),
                    initial: agent.context_window().to_string(),
                },
            },
            PickerItem {
                label: "Skill directories".into(),
                description: format!(
                    "{} configured · add or remove",
                    agent.config().skill_dirs.len()
                ),
                action: PickerAction::EditSetting {
                    key: "skills".into(),
                    initial: "add ".into(),
                },
            },
            PickerItem {
                label: "OpenAI-compatible provider".into(),
                description: "Add or update a provider".into(),
                action: PickerAction::EditSetting {
                    key: "provider".into(),
                    initial: String::new(),
                },
            },
            PickerItem {
                label: "OpenAI endpoint".into(),
                description: agent.config().openai_base_url.clone(),
                action: PickerAction::EditSetting {
                    key: "openai_base_url".into(),
                    initial: agent.config().openai_base_url.clone(),
                },
            },
            PickerItem {
                label: "Anthropic endpoint".into(),
                description: agent.config().anthropic_base_url.clone(),
                action: PickerAction::EditSetting {
                    key: "anthropic_base_url".into(),
                    initial: agent.config().anthropic_base_url.clone(),
                },
            },
            PickerItem {
                label: "Reload configuration".into(),
                description: "Read global and project files again".into(),
                action: PickerAction::Reload,
            },
            PickerItem {
                label: "Configuration details".into(),
                description: "Show paths, providers, and all commands".into(),
                action: PickerAction::ShowSettings,
            },
        ],
        editing: None,
    }
}

fn color_picker(current: UiColor) -> Picker {
    let choices = [
        ("White", UiColor::WHITE),
        ("Gray", UiColor::new(148, 148, 158)),
        ("Red", UiColor::new(235, 111, 146)),
        ("Orange", UiColor::new(240, 160, 96)),
        ("Yellow", UiColor::new(232, 202, 118)),
        ("Green", UiColor::new(139, 213, 162)),
        ("Cyan", UiColor::new(116, 199, 213)),
        ("Blue", UiColor::new(117, 169, 255)),
        ("Purple", UiColor::new(190, 149, 255)),
        ("Pink", UiColor::new(238, 148, 200)),
    ];
    let mut items = choices
        .into_iter()
        .map(|(label, color)| PickerItem {
            label: label.into(),
            description: format!(
                "\x1b[48;2;{};{};{}m   \x1b[0m {}",
                color.red,
                color.green,
                color.blue,
                color.config_value()
            ),
            action: PickerAction::SetAccentColor(color),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Custom RGB…".into(),
        description: "Enter #RRGGBB".into(),
        action: PickerAction::EditSetting {
            key: "accent_color".into(),
            initial: current.config_value(),
        },
    });
    let selected = items
        .iter()
        .position(|item| {
            matches!(
                item.action,
                PickerAction::SetAccentColor(color) if color == current
            )
        })
        .unwrap_or(items.len().saturating_sub(1));
    Picker {
        title: "Accent color".into(),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
    }
}

fn open_reasoning_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    state.picker = Some(reasoning_picker(agent, save));
}

fn reasoning_picker(agent: &Agent, save: bool) -> Picker {
    let target = crate::model::ModelTarget::parse(agent.model(), agent.config());
    let model = target.model();
    let current = agent.config().reasoning_effort.as_deref();
    let mut items = vec![PickerItem {
        label: "Provider default".into(),
        description: "Do not request a specific effort".into(),
        action: PickerAction::SetReasoning { effort: None, save },
    }];
    items.extend(
        crate::model::reasoning_efforts(agent.config(), agent.model())
            .iter()
            .map(|effort| PickerItem {
                label: title_case_effort(effort),
                description: reasoning_description(effort).into(),
                action: PickerAction::SetReasoning {
                    effort: Some((*effort).to_string()),
                    save,
                },
            }),
    );
    let selected = current
        .and_then(|current| {
            items.iter().position(|item| {
                matches!(
                    &item.action,
                    PickerAction::SetReasoning { effort: Some(effort), .. } if effort == current
                )
            })
        })
        .unwrap_or(0);
    Picker {
        title: format!("Reasoning · {model}"),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
        editing: None,
    }
}

fn title_case_effort(effort: &str) -> String {
    let mut chars = effort.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn reasoning_description(effort: &str) -> &'static str {
    match effort {
        "minimal" => "Fastest, least deliberation",
        "low" => "Fast with light deliberation",
        "medium" => "Balanced speed and depth",
        "high" => "More thorough reasoning",
        "xhigh" => "Very thorough reasoning",
        "max" => "Maximum available reasoning",
        _ => "",
    }
}

fn picker_is_editing(state: &ViewState) -> bool {
    state
        .picker
        .as_ref()
        .is_some_and(|picker| picker.editing.is_some())
}

fn take_picker_action(
    state: &mut ViewState,
    editor: &mut Editor,
    key: Key,
) -> Option<PickerAction> {
    let picker = state.picker.as_mut()?;

    if let Some(editing) = picker.editing.clone() {
        match key {
            Key::Escape | Key::Ctrl('c') => {
                editor.clear();
                picker.editing = None;
            }
            Key::Enter => {
                if let Some(value) = editor.take_text() {
                    let selected = match &editing {
                        PickerEdit::Setting(key) if key == "accent_color" => {
                            SETTINGS_ACCENT_COLOR_INDEX
                        }
                        _ => picker.selected,
                    };
                    state.picker = None;
                    return Some(match editing {
                        PickerEdit::Setting(key) => PickerAction::ApplySetting {
                            argument: format!("{key} {}", value.trim()),
                            selected,
                        },
                        PickerEdit::Model { save } => {
                            if save {
                                PickerAction::SaveModel(value.trim().to_string())
                            } else {
                                PickerAction::SwitchModel(value.trim().to_string())
                            }
                        }
                    });
                }
            }
            Key::Up | Key::Down => {}
            _ => {
                let _ = editor.handle_key(key);
            }
        }
        return None;
    }

    match key {
        Key::Escape | Key::Ctrl('c') => state.picker = None,
        Key::Up | Key::Char('k') => picker.selected = picker.selected.saturating_sub(1),
        Key::Down | Key::Char('j') => {
            picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1));
        }
        Key::PageUp => picker.selected = picker.selected.saturating_sub(5),
        Key::PageDown => {
            picker.selected = (picker.selected + 5).min(picker.items.len().saturating_sub(1));
        }
        Key::Enter => {
            let action = picker
                .items
                .get(picker.selected)
                .map(|item| item.action.clone());
            match action {
                Some(PickerAction::EditSetting { key, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Setting(key));
                }
                Some(PickerAction::EditModel { save, initial }) => {
                    editor.clear();
                    editor.paste(&initial);
                    picker.editing = Some(PickerEdit::Model { save });
                }
                Some(action) => {
                    state.picker = None;
                    return Some(action);
                }
                None => {}
            }
        }
        _ => {}
    }
    None
}

fn select_picker_item(state: &mut ViewState, selected: usize) {
    if let Some(picker) = state.picker.as_mut() {
        picker.selected = selected.min(picker.items.len().saturating_sub(1));
    }
}

fn queue_picker(state: &ViewState, selected: usize) -> Option<Picker> {
    if state.queued_inputs.is_empty() {
        return None;
    }
    let mut items = state
        .queued_inputs
        .iter()
        .enumerate()
        .map(|(index, input)| PickerItem {
            label: format!("Queued {}", index + 1),
            description: input.replace('\n', " "),
            action: PickerAction::RemoveQueued(index),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Clear all queued messages".into(),
        description: format!("Remove all {} pending", state.queued_inputs.len()),
        action: PickerAction::ClearQueued,
    });
    Some(Picker {
        title: "Queued messages".into(),
        hint: "↑/↓ move  Enter remove  Esc close".into(),
        selected: selected.min(items.len().saturating_sub(1)),
        items,
        editing: None,
    })
}

fn open_queue_picker(state: &mut ViewState) {
    state.picker = queue_picker(state, 0);
    if state.picker.is_none() {
        state.activity = "no queued messages".into();
    }
}

fn remove_queued(state: &mut ViewState, index: usize) -> bool {
    if state.queued_inputs.remove(index).is_some() {
        state.activity = format!("removed queued message {}", index + 1);
        state.scroll_offset = 0;
        true
    } else {
        state.activity = format!("queued message {} does not exist", index + 1);
        false
    }
}

fn clear_queued(state: &mut ViewState) {
    let count = state.queued_inputs.len();
    state.queued_inputs.clear();
    state.activity = match count {
        0 => "no queued messages".into(),
        1 => "removed 1 queued message".into(),
        _ => format!("removed {count} queued messages"),
    };
    state.scroll_offset = 0;
}

fn unqueue(argument: &str, state: &mut ViewState) {
    match argument {
        "" => open_queue_picker(state),
        "all" => clear_queued(state),
        number => match number
            .parse::<usize>()
            .ok()
            .and_then(|number| number.checked_sub(1))
        {
            Some(index) => {
                let _ = remove_queued(state, index);
            }
            None => state.activity = "usage: /unqueue [NUMBER|all]".into(),
        },
    }
}

fn handle_queue_picker_action(state: &mut ViewState, action: PickerAction) -> Option<PickerAction> {
    match action {
        PickerAction::RemoveQueued(index) => {
            if remove_queued(state, index) {
                state.picker = queue_picker(state, index);
            }
            None
        }
        PickerAction::ClearQueued => {
            clear_queued(state);
            None
        }
        action => Some(action),
    }
}

fn activate_picker_action(agent: &mut Agent, state: &mut ViewState, action: PickerAction) {
    let Some(action) = handle_queue_picker_action(state, action) else {
        return;
    };
    match action {
        PickerAction::SwitchModel(model) => {
            agent.switch_model(model);
            state.model = agent.model().to_string();
            state.context_window = agent.context_window();
            state.context_tokens = 0;
            if crate::model::is_codex(agent.config(), agent.model()) {
                open_reasoning_picker(agent, state, false);
            } else {
                state.notice(format!("Switched to {}.", agent.model()));
            }
        }
        PickerAction::SaveModel(model) => {
            if settings(agent, &format!("model {model}"), state) {
                if crate::model::is_codex(agent.config(), agent.model()) {
                    open_reasoning_picker(agent, state, true);
                } else {
                    open_settings_picker(agent, state);
                    select_picker_item(state, 0);
                }
            }
        }
        PickerAction::OpenModels { save } => open_model_picker(agent, state, save),
        PickerAction::OpenReasoning { save } => open_reasoning_picker(agent, state, save),
        PickerAction::SetReasoning { effort, save } => {
            if save {
                let value = effort.as_deref().unwrap_or("default");
                if settings(agent, &format!("reasoning_effort {value}"), state) {
                    open_settings_picker(agent, state);
                    select_picker_item(state, 2);
                }
            } else {
                agent.set_reasoning_effort(effort.clone());
                state.reasoning_effort = effort;
                let label = state
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("provider default");
                state.notice(format!("Using {} with {label} reasoning.", agent.model()));
            }
        }
        PickerAction::SetHideReasoning(enabled) => {
            if settings(
                agent,
                &format!("hide_reasoning {}", if enabled { "on" } else { "off" }),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, 3);
            }
        }
        PickerAction::OpenAccentColor => {
            state.picker = Some(color_picker(agent.config().accent_color));
        }
        PickerAction::SetAccentColor(color) => {
            if settings(
                agent,
                &format!("accent_color {}", color.config_value()),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_ACCENT_COLOR_INDEX);
            }
        }
        PickerAction::ResumeSession(id) => load_session(agent, &id, state),
        PickerAction::ApplySetting { argument, selected } => {
            if settings(agent, &argument, state) {
                open_settings_picker(agent, state);
                select_picker_item(state, selected);
            }
        }
        PickerAction::SetAutoCompact(enabled) => {
            if settings(
                agent,
                &format!("auto_compact {}", if enabled { "on" } else { "off" }),
                state,
            ) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_AUTO_COMPACT_INDEX);
            }
        }
        PickerAction::Reload => {
            if settings(agent, "reload", state) {
                open_settings_picker(agent, state);
                select_picker_item(state, SETTINGS_RELOAD_INDEX);
            }
        }
        PickerAction::ShowSettings => show_settings(agent, state),
        PickerAction::EditSetting { .. } | PickerAction::EditModel { .. } => {}
        PickerAction::RemoveQueued(_) | PickerAction::ClearQueued => {}
    }
    state.refresh_completions(agent);
}

fn settings(agent: &mut Agent, argument: &str, state: &mut ViewState) -> bool {
    if argument.is_empty() {
        show_settings(agent, state);
        return false;
    }

    let mut parts = argument.split_whitespace();
    let key = parts.next().unwrap_or_default();
    let change = match key {
        "reload" => {
            if parts.next().is_some() {
                Err(Error::Config("usage: /settings reload".into()))
            } else {
                Ok(ConfigChange::Reload)
            }
        }
        "model" => one_value(&mut parts, "usage: /settings model MODEL")
            .map(|model| ConfigChange::Model(model.to_string())),
        "max_tokens" => one_value(&mut parts, "usage: /settings max_tokens NUMBER")
            .map(|value| ConfigChange::MaxTokens(value.to_string())),
        "reasoning_effort" => one_value(
            &mut parts,
            "usage: /settings reasoning_effort default|minimal|low|medium|high|xhigh|max",
        )
        .map(|value| ConfigChange::ReasoningEffort(value.to_string())),
        "hide_reasoning" => one_value(&mut parts, "usage: /settings hide_reasoning on|off")
            .map(|value| ConfigChange::HideReasoning(value.to_string())),
        "accent_color" | "status_bar_color" | "text_box_color" => {
            one_value(&mut parts, "usage: /settings accent_color NAME|#RRGGBB")
                .map(|value| ConfigChange::AccentColor(value.to_string()))
        }
        "auto_compact" => one_value(&mut parts, "usage: /settings auto_compact on|off")
            .map(|value| ConfigChange::AutoCompact(value.to_string())),
        "compact_threshold" => one_value(
            &mut parts,
            "usage: /settings compact_threshold FRACTION|PERCENT%",
        )
        .map(|value| ConfigChange::CompactThreshold(value.to_string())),
        "context_window" => {
            one_value(&mut parts, "usage: /settings context_window TOKENS").map(|value| {
                ConfigChange::ContextWindow {
                    model: agent.model().to_string(),
                    value: value.to_string(),
                }
            })
        }
        "skills" => {
            let action = parts.next();
            let path = parts.next();
            if !matches!(action, Some("add" | "remove")) || path.is_none() || parts.next().is_some()
            {
                Err(Error::Config(
                    "usage: /settings skills add|remove DIRECTORY".into(),
                ))
            } else {
                Ok(ConfigChange::SkillDirectory {
                    action: if action == Some("add") {
                        SkillDirectoryAction::Add
                    } else {
                        SkillDirectoryAction::Remove
                    },
                    path: path.unwrap_or_default().to_string(),
                })
            }
        }
        "anthropic_base_url" | "openai_base_url" => {
            one_value(&mut parts, "usage: /settings openai_base_url URL").map(|url| {
                if key == "anthropic_base_url" {
                    ConfigChange::AnthropicBaseUrl(url.to_string())
                } else {
                    ConfigChange::OpenAiBaseUrl(url.to_string())
                }
            })
        }
        "provider" => {
            let name = parts.next();
            let url = parts.next();
            let api_key = parts.next();
            if name.is_none() || url.is_none() || parts.next().is_some() {
                Err(Error::Config(
                    "usage: /settings provider NAME BASE_URL [API_KEY|-]".into(),
                ))
            } else {
                Ok(ConfigChange::Provider {
                    name: name.unwrap_or_default().to_string(),
                    base_url: url.unwrap_or_default().to_string(),
                    api_key: api_key.map(str::to_string),
                })
            }
        }
        _ => Err(Error::Config(format!(
            "unknown setting '{key}'; run /settings to list settings"
        ))),
    };

    let result = change.and_then(|change| agent.change_global_config(change));

    match result {
        Ok(effect) => {
            state.model = agent.model().to_string();
            state.reasoning_effort = agent.config().reasoning_effort.clone();
            state.hide_reasoning = agent.config().hide_reasoning;
            state.accent_color = agent.config().accent_color;
            state.context_window = agent.context_window();
            match effect {
                ConfigChangeEffect::Applied => state.notice(format!(
                    "Saved to `{}` and applied.",
                    agent.config().global_config_path().display()
                )),
                ConfigChangeEffect::Overridden => state.notice(format!(
                    "Saved to `{}`, but project settings in `{}` remain effective.",
                    agent.config().global_config_path().display(),
                    agent.config().project_config_path().display()
                )),
                ConfigChangeEffect::SkillDirectoryNotConfigured(path) => state.notice(format!(
                    "Skill directory `{}` is not configured.",
                    path.display()
                )),
            }
            true
        }
        Err(error) => {
            state.notice(format!("Could not change setting: {error}"));
            false
        }
    }
}

fn show_settings(agent: &Agent, state: &mut ViewState) {
    let mut providers = agent.config().providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|(name, _)| name.as_str());
    let mut text = format!(
        "Settings\n\n- model: `{}`\n- max_tokens: `{}`\n- reasoning_effort: `{}`\n- hide_reasoning: `{}`\n- accent_color: `{}`\n- auto_compact: `{}`\n- compact_threshold: `{:.0}%`\n- context_window for current model: `{}`\n- anthropic_base_url: `{}`\n- openai_base_url: `{}`\n\nSkill directories\n\n",
        agent.model(),
        agent.config().max_tokens,
        agent
            .config()
            .reasoning_effort
            .as_deref()
            .unwrap_or("provider default"),
        agent.config().hide_reasoning,
        agent.config().accent_color.config_value(),
        if agent.config().auto_compact {
            "on"
        } else {
            "off"
        },
        agent.config().compact_threshold * 100.0,
        agent.context_window(),
        agent.config().anthropic_base_url,
        agent.config().openai_base_url,
    );
    for dir in &agent.config().skill_dirs {
        text.push_str(&format!("- `{}`\n", dir.display()));
    }
    text.push_str("\nOpenAI-compatible providers\n\n");
    for (name, provider) in providers {
        let auth = if provider.api_key.is_some() {
            "configured key"
        } else {
            "no configured key"
        };
        text.push_str(&format!(
            "- `{name}`: `{}` ({auth}, {} listed models)\n",
            provider.base_url,
            provider.models.len()
        ));
    }
    text.push_str(&format!(
        "\nChanges are written to `{}`. Project settings in `./.yawl/config.json` override them.\n\nCommands\n\n- `/settings model MODEL`\n- `/settings max_tokens NUMBER`\n- `/settings reasoning_effort default|minimal|low|medium|high|xhigh|max`\n- `/settings hide_reasoning on|off`\n- `/settings accent_color NAME|#RRGGBB`\n- `/settings auto_compact on|off`\n- `/settings compact_threshold 85%`\n- `/settings context_window TOKENS`\n- `/settings skills add|remove DIRECTORY`\n- `/settings provider NAME BASE_URL [API_KEY|-]`\n- `/settings openai_base_url URL`\n- `/settings anthropic_base_url URL`\n- `/settings reload`\n\nUse an environment reference such as `$OMLX_API_KEY` instead of putting a secret directly in terminal history. Pass `-` as the provider key to remove a saved key.",
        agent.config().global_config_path().display()
    ));
    state.notice(text);
}

fn one_value<'a>(parts: &mut impl Iterator<Item = &'a str>, usage: &str) -> Result<&'a str, Error> {
    let value = parts
        .next()
        .ok_or_else(|| Error::Config(usage.to_string()))?;
    if parts.next().is_some() {
        return Err(Error::Config(usage.to_string()));
    }
    Ok(value)
}

fn open_resume_picker(agent: &Agent, state: &mut ViewState) {
    let sessions = match crate::session::list(&agent.config().sessions_dir()) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if sessions.is_empty() {
        state.notice("No saved sessions.");
        return;
    }
    state.picker = Some(Picker {
        title: "Resume session".into(),
        hint: "↑/↓ move  Enter resume  Esc cancel".into(),
        selected: 0,
        items: sessions
            .into_iter()
            .take(100)
            .map(|session| PickerItem {
                label: if session.preview.is_empty() {
                    "Untitled session".into()
                } else {
                    session.preview
                },
                description: session.id.clone(),
                action: PickerAction::ResumeSession(session.id),
            })
            .collect(),
        editing: None,
    });
}

fn resume(agent: &mut Agent, selector: &str, state: &mut ViewState) {
    let sessions = match crate::session::list(&agent.config().sessions_dir()) {
        Ok(sessions) => sessions,
        Err(error) => {
            state.notice(format!("Could not list sessions: {error}"));
            return;
        }
    };
    if selector.is_empty() {
        if sessions.is_empty() {
            state.notice("No saved sessions.");
            return;
        }
        let mut text = String::from("Saved sessions\n\n");
        for (index, session) in sessions.iter().take(20).enumerate() {
            text.push_str(&format!(
                "{}. `{}`  {}\n",
                index + 1,
                session.id,
                session.preview
            ));
        }
        text.push_str("\nUse `/resume ID` or `/resume NUMBER`.");
        state.notice(text);
        return;
    }
    let id = selector
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .and_then(|index| sessions.get(index))
        .map_or(selector, |session| session.id.as_str());
    load_session(agent, id, state);
}

fn load_session(agent: &mut Agent, id: &str, state: &mut ViewState) {
    match agent.load_session(id) {
        Ok(()) => {
            let queued_inputs = std::mem::take(&mut state.queued_inputs);
            let pending_actions = std::mem::take(&mut state.pending_actions);
            *state = ViewState::from_agent(agent);
            state.queued_inputs = queued_inputs;
            state.pending_actions = pending_actions;
            state.notice(format!("Resumed session {id}."));
        }
        Err(error) => state.notice(format!("Could not resume '{id}': {error}")),
    }
}

fn turn_interactive<R: Read>(
    agent: &mut Agent,
    input: String,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<bool, Error> {
    let active_pickers = ActivePickers::from_agent(agent);
    std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = agent.run_turn(Some(input), &mut |event| {
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
            },
            state,
            editor,
            terminal,
            events,
            &active_pickers,
        )
    })
}

fn compact_interactive<R: Read>(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<(), Error> {
    let active_pickers = ActivePickers::from_agent(agent);
    std::thread::scope(|scope| {
        let (updates_tx, updates_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let (thread_tx, thread_rx) = mpsc::channel();
        scope.spawn(move || {
            let _ = thread_tx.send(native_thread_id());
            let result = agent.compact_now(&mut |event| {
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
            },
            state,
            editor,
            terminal,
            events,
            &active_pickers,
        )
    })
}

struct WorkerChannels<T> {
    updates: Receiver<Update>,
    done: Receiver<Result<T, Error>>,
    thread: usize,
}

fn pump_events<R: Read, T>(
    worker: WorkerChannels<T>,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
    active_pickers: &ActivePickers,
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
        if state.picker.is_some() {
            match event {
                Event::Key(key) if is_cancel_key(key) => {
                    state.picker = None;
                    cancel_worker(worker.thread, state);
                }
                Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
                Event::Key(key) => {
                    if let Some(action) = take_picker_action(state, editor, key) {
                        activate_picker_action_while_busy(state, action, active_pickers);
                    }
                }
                Event::Paste(text) if picker_is_editing(state) => editor.paste(&text),
                Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
                Event::Tick => advance_copy_toast(state),
                Event::MouseScroll(_) | Event::Paste(_) => {}
            }
            continue;
        }
        match event {
            Event::Tick => {
                advance_copy_toast(state);
                if crate::interrupted() {
                    state.activity = "canceling turn".into();
                }
            }
            Event::MouseScroll(amount) => scroll(state, amount),
            Event::Mouse(mouse) => handle_mouse_selection(terminal, state, mouse)?,
            Event::Paste(text) => editor.paste(&text),
            Event::Key(key) if is_cancel_key(key) => cancel_worker(worker.thread, state),
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

fn handle_mouse_selection(
    terminal: &mut Terminal,
    state: &mut ViewState,
    event: MouseEvent,
) -> Result<(), Error> {
    if terminal.handle_mouse(event)? {
        state.copy_toast_ticks = COPY_TOAST_TICKS;
    }
    Ok(())
}

fn advance_copy_toast(state: &mut ViewState) {
    state.copy_toast_ticks = state.copy_toast_ticks.saturating_sub(1);
}

fn is_cancel_key(key: Key) -> bool {
    matches!(key, Key::Escape | Key::Ctrl('c'))
}

fn cancel_worker(thread: usize, state: &mut ViewState) {
    crate::set_interrupted(true);
    interrupt_thread(thread);
    state.activity = "canceling turn".into();
}

fn handle_submission_while_busy(
    input: String,
    state: &mut ViewState,
    active_pickers: &ActivePickers,
) {
    match busy_command(&input) {
        Some(BusyCommand::Settings) => state.picker = Some(active_pickers.settings.clone()),
        Some(BusyCommand::Model) => state.picker = Some(active_pickers.model.clone()),
        Some(BusyCommand::Unqueue(argument)) => unqueue(&argument, state),
        None => {
            state.queued_inputs.push_back(input);
            state.scroll_offset = 0;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum BusyCommand {
    Settings,
    Model,
    Unqueue(String),
}

fn busy_command(input: &str) -> Option<BusyCommand> {
    let command = input.trim().strip_prefix('/')?;
    let (name, argument) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(name, argument)| (name, argument.trim()));
    match name {
        "settings" if argument.is_empty() => Some(BusyCommand::Settings),
        "model" if argument.is_empty() => Some(BusyCommand::Model),
        "unqueue" => Some(BusyCommand::Unqueue(argument.to_string())),
        _ => None,
    }
}

fn activate_picker_action_while_busy(
    state: &mut ViewState,
    action: PickerAction,
    active_pickers: &ActivePickers,
) {
    let Some(action) = handle_queue_picker_action(state, action) else {
        return;
    };
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

fn native_thread_id() -> usize {
    // SAFETY: `pthread_self` takes no arguments and returns the calling
    // thread's stable pthread identifier.
    unsafe { libc::pthread_self() as usize }
}

fn interrupt_thread(thread: usize) {
    // SAFETY: `thread` came from `pthread_self` in the live scoped worker.
    // The installed SIGINT handler only sets the shared interrupt flag.
    unsafe {
        libc::pthread_kill(thread as libc::pthread_t, libc::SIGINT);
    }
}

fn scroll(state: &mut ViewState, amount: i32) {
    if amount >= 0 {
        state.scroll_offset = state.scroll_offset.saturating_add(amount as usize);
    } else {
        state.scroll_offset = state
            .scroll_offset
            .saturating_sub(amount.unsigned_abs() as usize);
    }
}

fn toggle_tool_expansion(state: &mut ViewState) {
    state.tools_expanded = !state.tools_expanded;
    state.scroll_offset = 0;
    state.activity = if state.tools_expanded {
        "tool output expanded".into()
    } else {
        "tool output compact".into()
    };
}

fn render_entries(
    entries: &[Entry],
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry {
            Entry::User(content) => lines.extend(render_user_panel(content, width)),
            Entry::Assistant(content) => {
                if content.trim().is_empty() {
                    continue;
                }
                lines.extend(markdown::render(content.trim(), width));
            }
            Entry::Reasoning { kind, content } => {
                if hide_reasoning || content.trim().is_empty() {
                    continue;
                }
                lines.extend(render_reasoning(*kind, content, width));
            }
            Entry::Tool {
                name,
                args,
                output,
                is_error,
                running,
            } => lines.extend(tool_view::render(
                name,
                args,
                output,
                *is_error,
                *running,
                width,
                tools_expanded,
            )),
            Entry::Notice(content) => {
                lines.push("\x1b[1;33mYawl\x1b[0m".into());
                lines.extend(markdown::render(content, width));
            }
        }
        lines.push(String::new());
    }
    lines
}

fn render_reasoning(kind: ReasoningKind, content: &str, width: usize) -> Vec<String> {
    const STYLE: &str = "\x1b[2;3;38;2;148;148;158m";
    let continuation = format!("\x1b[0m{STYLE}");
    let style = |line: String| format!("{STYLE}{}\x1b[0m", line.replace("\x1b[0m", &continuation));
    match kind {
        ReasoningKind::Summary => {
            let summary = content.split_whitespace().collect::<Vec<_>>().join(" ");
            vec![style(markdown::fit_width(&summary, width))]
        }
        ReasoningKind::Full => markdown::render(content.trim(), width)
            .into_iter()
            .map(style)
            .collect(),
    }
}

fn render_user_panel(content: &str, width: usize) -> Vec<String> {
    let panel_width = width.max(1);
    let horizontal_padding = usize::from(panel_width >= 3);
    let content_width = panel_width.saturating_sub(horizontal_padding * 2).max(1);
    let blank = format!("{USER_BACKGROUND}{}\x1b[0m", " ".repeat(panel_width));
    let continuation = format!("\x1b[0m{USER_BACKGROUND}{USER_TEXT}");
    let mut lines = Vec::new();
    lines.push(blank.clone());
    lines.extend(
        markdown::render(content, content_width)
            .into_iter()
            .map(|line| {
                let fitted =
                    markdown::fit_width(&line, content_width).replace("\x1b[0m", &continuation);
                format!(
                    "{USER_BACKGROUND}{USER_TEXT}{}{fitted}{}\x1b[0m",
                    " ".repeat(horizontal_padding),
                    " ".repeat(horizontal_padding)
                )
            }),
    );
    lines.push(blank);
    lines
}

fn render_queued_panel(content: &str, position: usize, width: usize) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;33mQueued {position} · waiting for the active response\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

fn foreground_color(color: UiColor) -> String {
    format!("\x1b[38;2;{};{};{}m", color.red, color.green, color.blue)
}

fn status_style(color: UiColor) -> String {
    let luminance =
        u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114;
    let text = if luminance >= 150_000 { 24 } else { 245 };
    format!(
        "\x1b[38;2;{text};{text};{text};48;2;{};{};{}m",
        color.red, color.green, color.blue
    )
}

fn render_copy_toast(frame: &mut [String], columns: usize, accent: UiColor) {
    const WIDTH: usize = 11;
    let color = foreground_color(accent);
    let toast = [
        format!("{color}┌─────────┐\x1b[0m"),
        format!("{color}│\x1b[0m Copied! {color}│\x1b[0m"),
        format!("{color}└─────────┘\x1b[0m"),
    ];
    let left_width = columns.saturating_sub(WIDTH);
    for (line, toast_line) in frame.iter_mut().zip(toast) {
        *line = format!("{}{toast_line}", markdown::fit_width(line, left_width));
    }
}

struct Terminal {
    original: libc::termios,
    stdout: io::Stdout,
    active: bool,
    last_frame: Vec<String>,
    last_base_frame: Vec<String>,
    last_size: (u16, u16),
    selection: Option<TextSelection>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ScreenPoint {
    row: usize,
    column: usize,
}

struct TextSelection {
    anchor: ScreenPoint,
    current: ScreenPoint,
    frame: Vec<String>,
}

impl Terminal {
    fn enter() -> Result<Self, Error> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(Error::Config("the terminal UI needs a TTY".into()));
        }
        // SAFETY: A zeroed termios value is immediately initialized by
        // `tcgetattr` before any field is read.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: STDIN_FILENO is valid for this process and `original`
        // points to writable termios storage.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut raw = original;
        // SAFETY: `raw` is an initialized termios value.
        unsafe { libc::cfmakeraw(&mut raw) };
        // Keep legacy Ctrl+C as SIGINT. Disable the other signal-generating
        // control characters because suspending would leave the terminal in
        // raw mode.
        raw.c_lflag |= libc::ISIG;
        raw.c_cc[libc::VQUIT] = 0;
        raw.c_cc[libc::VSUSP] = 0;
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // SAFETY: STDIN_FILENO is valid and `raw` points to initialized
        // termios storage for this terminal.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw) } != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }

        let mut terminal = Self {
            original,
            stdout: io::stdout(),
            active: true,
            last_frame: Vec::new(),
            last_base_frame: Vec::new(),
            last_size: (0, 0),
            selection: None,
        };
        terminal.stdout.write_all(
            b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[>1u",
        )?;
        terminal.stdout.flush()?;
        Ok(terminal)
    }

    fn invalidate(&mut self) {
        self.last_frame.clear();
        self.last_base_frame.clear();
        self.last_size = (0, 0);
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Result<bool, Error> {
        let point = ScreenPoint {
            row: event.row,
            column: event.column,
        };
        match event.kind {
            MouseKind::Press => {
                if self.last_base_frame.is_empty() {
                    return Ok(false);
                }
                let point = clamp_point(point, &self.last_base_frame);
                self.selection = Some(TextSelection {
                    anchor: point,
                    current: point,
                    frame: self.last_base_frame.clone(),
                });
                Ok(false)
            }
            MouseKind::Drag => {
                if let Some(selection) = self.selection.as_mut() {
                    selection.current = clamp_point(point, &selection.frame);
                }
                Ok(false)
            }
            MouseKind::Release => {
                let Some(mut selection) = self.selection.take() else {
                    return Ok(false);
                };
                selection.current = clamp_point(point, &selection.frame);
                let text = selected_text(&selection);
                if text.is_empty() {
                    return Ok(false);
                }
                if !copy_with_platform_command(&text) {
                    let encoded = base64_encode(text.as_bytes());
                    write!(self.stdout, "\x1b]52;c;{encoded}\x07")?;
                    self.stdout.flush()?;
                }
                Ok(true)
            }
        }
    }

    fn draw(&mut self, state: &mut ViewState, editor: &Editor) -> Result<(), Error> {
        let (columns, rows) = terminal_size();
        let (base_frame, cursor) =
            build_frame(state, editor, usize::from(columns), usize::from(rows));
        self.last_base_frame.clone_from(&base_frame);
        let frame = self
            .selection
            .as_ref()
            .map_or(base_frame, highlighted_selection);
        let force = self.last_size != (columns, rows) || self.last_frame.len() != frame.len();
        self.stdout.write_all(b"\x1b[?25l")?;
        if force {
            self.stdout.write_all(b"\x1b[2J")?;
        }
        for (index, line) in frame.iter().enumerate() {
            if force || self.last_frame.get(index) != Some(line) {
                write!(self.stdout, "\x1b[{};1H\x1b[2K{line}", index + 1)?;
            }
        }
        if self.selection.is_some() {
            self.stdout.write_all(b"\x1b[?25l")?;
        } else {
            write!(self.stdout, "\x1b[{};{}H\x1b[?25h", cursor.0, cursor.1)?;
        }
        self.stdout.flush()?;
        self.last_frame = frame;
        self.last_size = (columns, rows);
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = self.stdout.write_all(
            b"\x1b[<u\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[0m\x1b[?1049l",
        );
        let _ = self.stdout.flush();
        // SAFETY: `original` came from a successful `tcgetattr` call for
        // STDIN_FILENO and remains initialized for the life of this guard.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
        self.active = false;
    }
}

fn clamp_point(point: ScreenPoint, frame: &[String]) -> ScreenPoint {
    let row = point.row.min(frame.len().saturating_sub(1));
    let width = frame
        .get(row)
        .map_or(1, |line| markdown::visible_width(line).max(1));
    ScreenPoint {
        row,
        column: point.column.min(width.saturating_sub(1)),
    }
}

fn selected_text(selection: &TextSelection) -> String {
    if selection.anchor == selection.current {
        return String::new();
    }
    let (start, end) = if selection.anchor <= selection.current {
        (selection.anchor, selection.current)
    } else {
        (selection.current, selection.anchor)
    };
    selection.frame[start.row..=end.row]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let plain = markdown::strip_ansi(line);
            let line_length = plain.chars().count();
            let row = start.row + offset;
            let from = if row == start.row { start.column } else { 0 };
            let through = if row == end.row {
                end.column.saturating_add(1)
            } else {
                line_length
            };
            plain
                .chars()
                .skip(from.min(line_length))
                .take(
                    through
                        .min(line_length)
                        .saturating_sub(from.min(line_length)),
                )
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_string()
}

fn highlighted_selection(selection: &TextSelection) -> Vec<String> {
    let (start, end) = if selection.anchor <= selection.current {
        (selection.anchor, selection.current)
    } else {
        (selection.current, selection.anchor)
    };
    let mut frame = selection.frame.clone();
    for (row, line) in frame
        .iter_mut()
        .enumerate()
        .take(end.row + 1)
        .skip(start.row)
    {
        let width = markdown::visible_width(line);
        let from = if row == start.row { start.column } else { 0 };
        let through = if row == end.row {
            end.column.saturating_add(1)
        } else {
            width
        };
        *line = highlight_cells(line, from.min(width), through.min(width));
    }
    frame
}

fn highlight_cells(line: &str, from: usize, through: usize) -> String {
    if from >= through {
        return line.to_string();
    }
    let mut output = String::with_capacity(line.len() + 16);
    let mut index = 0usize;
    let mut column = 0usize;
    let mut highlighted = false;
    while index < line.len() {
        if column == from && !highlighted {
            output.push_str("\x1b[7m");
            highlighted = true;
        }
        if column == through && highlighted {
            output.push_str("\x1b[27m");
            highlighted = false;
        }
        if line.as_bytes()[index] == 0x1b {
            let search_start = (index + 2).min(line.len());
            let end = line.as_bytes()[search_start..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map_or(line.len(), |relative| search_start + relative + 1);
            output.push_str(&line[index..end]);
            if highlighted && line.as_bytes().get(end.saturating_sub(1)) == Some(&b'm') {
                output.push_str("\x1b[7m");
            }
            index = end;
            continue;
        }
        let character = line[index..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        output.push(character);
        index += character.len_utf8();
        column += 1;
    }
    if highlighted {
        output.push_str("\x1b[27m");
    }
    output
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from((first & 0x03) << 4 | second >> 4)],
        ));
        encoded.push(if chunk.len() > 1 {
            char::from(ALPHABET[usize::from((second & 0x0f) << 2 | third >> 6)])
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            char::from(ALPHABET[usize::from(third & 0x3f)])
        } else {
            '='
        });
    }
    encoded
}

fn copy_command(program: &str, arguments: &[&str], text: &str) -> bool {
    let Ok(mut child) = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    let succeeded = child.wait().is_ok_and(|status| status.success());
    wrote && succeeded
}

#[cfg(target_os = "macos")]
fn copy_with_platform_command(text: &str) -> bool {
    copy_command("pbcopy", &[], text)
}

#[cfg(target_os = "linux")]
fn copy_with_platform_command(text: &str) -> bool {
    copy_command("wl-copy", &[], text)
        || copy_command("xclip", &["-selection", "clipboard"], text)
        || copy_command("xsel", &["--clipboard", "--input"], text)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn copy_with_platform_command(_text: &str) -> bool {
    false
}

fn terminal_size() -> (u16, u16) {
    // SAFETY: A zeroed winsize has a valid all-integer representation and is
    // passed to ioctl as writable storage.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO is a terminal while the TUI is active and `size`
    // points to writable winsize storage.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
        && size.ws_col > 0
        && size.ws_row > 0
    {
        (size.ws_col.max(20), size.ws_row.max(8))
    } else {
        (80, 24)
    }
}

fn render_picker(picker: &Picker, editor: &Editor, columns: usize, height: usize) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let box_width = columns.saturating_sub(4).clamp(16, 76);
    let inner = box_width.saturating_sub(2);
    let capacity = height
        .saturating_sub(5)
        .max(1)
        .min(picker.items.len().max(1));
    let mut start = picker.selected.saturating_sub(capacity / 2);
    start = start.min(picker.items.len().saturating_sub(capacity));
    let end = (start + capacity).min(picker.items.len());
    let left = " ".repeat(columns.saturating_sub(box_width) / 2);
    let boxed = |content: &str| format!("{left}│{}│", markdown::fit_width(content, inner));
    let mut panel = vec![format!("{left}┌{}┐", "─".repeat(inner))];
    panel.push(boxed(&format!(" \x1b[1m{}\x1b[0m", picker.title)));
    panel.push(format!("{left}├{}┤", "─".repeat(inner)));
    for (index, item) in picker.items[start..end].iter().enumerate() {
        let absolute = start + index;
        let marker = if absolute == picker.selected {
            "›"
        } else {
            " "
        };
        let description = if absolute == picker.selected && picker.editing.is_some() {
            let value = editor.text();
            if value.is_empty() {
                "type a value below…".into()
            } else {
                value.replace('\n', " ")
            }
        } else {
            item.description.clone()
        };
        let text = format!(" {marker} {}  ·  {description}", item.label);
        if absolute == picker.selected {
            panel.push(boxed(&format!(
                "\x1b[7m{}\x1b[0m",
                markdown::fit_width(&text, inner)
            )));
        } else {
            panel.push(boxed(&text));
        }
    }
    let hint = if picker.editing.is_some() {
        "Edit below  Enter save  Esc cancel edit"
    } else {
        &picker.hint
    };
    panel.push(boxed(&format!(" \x1b[2m{hint}\x1b[0m")));
    panel.push(format!("{left}└{}┘", "─".repeat(inner)));

    if panel.len() > height {
        panel.truncate(height);
    }
    let top = height.saturating_sub(panel.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.extend(std::iter::repeat_n(" ".repeat(columns), top));
    lines.extend(
        panel
            .into_iter()
            .map(|line| markdown::fit_width(&line, columns)),
    );
    lines.extend(std::iter::repeat_n(
        " ".repeat(columns),
        height.saturating_sub(lines.len()),
    ));
    lines
}

fn build_frame(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(20);
    let rows = rows.max(8);
    let inner_width = columns.saturating_sub(2);
    let layout = editor.layout(inner_width);
    let max_input_lines = (rows / 3).max(1);
    let input_start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(max_input_lines)
        .min(layout.lines.len().saturating_sub(max_input_lines));
    let input_end = (input_start + max_input_lines).min(layout.lines.len());
    let input_lines = &layout.lines[input_start..input_end];
    let cursor_input_row = layout.cursor_row.saturating_sub(input_start);
    let input_height = input_lines.len() + 2;
    let menu_capacity = rows.saturating_sub(input_height + 1);
    let match_count = if state.picker.is_none() {
        matching_completions(state, editor).len().min(menu_capacity)
    } else {
        0
    };
    if match_count > 0 {
        state.completion_index = state.completion_index.min(match_count - 1);
    }
    let menu = if state.picker.is_none() {
        matching_completions(state, editor)
            .iter()
            .take(menu_capacity)
            .enumerate()
            .map(|(index, completion)| {
                let line = format!("  {:<18} {}", completion.command, completion.description);
                if index == state.completion_index {
                    format!("\x1b[7m{}\x1b[0m", markdown::fit_width(&line, columns))
                } else {
                    markdown::fit_width(&line, columns)
                }
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let menu_height = menu.len();
    let transcript_height = rows.saturating_sub(input_height + menu_height + 1);
    let mut transcript = render_entries(
        state.transcript.entries(),
        columns,
        state.tools_expanded,
        state.hide_reasoning,
    );
    for (index, input) in state.queued_inputs.iter().enumerate() {
        transcript.extend(render_queued_panel(input, index + 1, columns));
    }
    let max_scroll = transcript.len().saturating_sub(transcript_height);
    state.scroll_offset = state.scroll_offset.min(max_scroll);
    let end = transcript.len().saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(transcript_height);
    let visible = &transcript[start..end];

    let mut frame = Vec::with_capacity(rows);
    if let Some(picker) = &state.picker {
        frame.extend(render_picker(picker, editor, columns, transcript_height));
    } else {
        frame.extend(std::iter::repeat_n(
            " ".repeat(columns),
            transcript_height.saturating_sub(visible.len()),
        ));
        frame.extend(
            visible
                .iter()
                .map(|line| markdown::fit_width(line, columns)),
        );
    }
    frame.extend(menu);
    let text_box_color = foreground_color(state.accent_color);
    frame.push(format!(
        "{text_box_color}┌{}┐\x1b[0m",
        "─".repeat(inner_width)
    ));
    for line in input_lines {
        frame.push(format!(
            "{text_box_color}│\x1b[0m{}{text_box_color}│\x1b[0m",
            markdown::fit_width(line, inner_width)
        ));
    }
    frame.push(format!(
        "{text_box_color}└{}┘\x1b[0m",
        "─".repeat(inner_width)
    ));

    let percentage = state
        .context_tokens
        .saturating_mul(100)
        .checked_div(state.context_window)
        .unwrap_or(0);
    let reasoning = state
        .reasoning_effort
        .as_deref()
        .map_or(String::new(), |effort| format!(" · {effort}"));
    let mut status = format!(
        " {}{}  {}/{} tokens ({}%)",
        state.model, reasoning, state.context_tokens, state.context_window, percentage
    );
    if !state.activity.is_empty() {
        status.push_str("  ");
        status.push_str(&state.activity);
    }
    if !state.queued_inputs.is_empty() {
        status.push_str(&format!("  {} queued", state.queued_inputs.len()));
    }
    if !state.pending_actions.is_empty() {
        status.push_str(&format!("  {} change pending", state.pending_actions.len()));
    }
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(state.accent_color),
        markdown::fit_width(&status, columns)
    ));

    if state.copy_toast_ticks > 0 {
        render_copy_toast(&mut frame, columns, state.accent_color);
    }

    let cursor_row = transcript_height + menu_height + 2 + cursor_input_row;
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_clear_are_new_session_commands() {
        assert!(is_new_session_command("new"));
        assert!(is_new_session_command("clear"));
        assert!(!is_new_session_command("compact"));
    }

    #[test]
    fn user_messages_render_in_a_padded_panel() {
        let rendered = render_entries(&[Entry::User("hello".into())], 24, false, false);
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert_eq!(rendered.len(), 4);
        assert!(
            rendered[..3].iter().all(
                |line| line.starts_with(USER_BACKGROUND) && markdown::visible_width(line) == 24
            )
        );
        assert!(plain.contains(" hello"));
        assert!(!plain.contains("You"));
    }

    #[test]
    fn assistant_messages_do_not_show_a_title() {
        let rendered = render_entries(&[Entry::Assistant("hello".into())], 24, false, false);
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("hello"));
        assert!(!plain.contains("Yawl"));
    }

    #[test]
    fn frame_keeps_input_and_status_pinned() {
        let mut state = ViewState {
            transcript: Transcript::from_messages(&[crate::provider::Message::assistant(
                "hello".into(),
                Vec::new(),
            )]),
            tools_expanded: false,
            model: "test".into(),
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            copy_toast_ticks: 0,
            context_tokens: 12,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            pending_actions: std::collections::VecDeque::new(),
            completions: Vec::new(),
            completion_index: 0,
            picker: None,
        };
        let editor = Editor::default();
        let (frame, cursor) = build_frame(&mut state, &editor, 40, 12);
        assert_eq!(frame.len(), 12);
        assert!(markdown::strip_ansi(frame.last().unwrap()).contains("test"));
        assert_eq!(cursor.0, 10);
        assert!(frame.last().unwrap().contains("48;2;238;238;238"));
        assert!(frame[8].contains("38;2;238;238;238"));

        state.copy_toast_ticks = 1;
        let (frame, _) = build_frame(&mut state, &editor, 40, 12);
        assert!(markdown::strip_ansi(&frame[1]).ends_with("│ Copied! │"));
        assert!(
            frame[..3]
                .iter()
                .all(|line| markdown::visible_width(line) == 40)
        );
        advance_copy_toast(&mut state);
        let (frame, _) = build_frame(&mut state, &editor, 40, 12);
        assert!(!markdown::strip_ansi(&frame.join("\n")).contains("Copied!"));
    }

    #[test]
    fn screen_selection_extracts_styled_text_in_either_direction() {
        let selection = TextSelection {
            anchor: ScreenPoint { row: 1, column: 5 },
            current: ScreenPoint { row: 0, column: 6 },
            frame: vec![
                "\x1b[31mhello world\x1b[0m   ".into(),
                "second line   ".into(),
            ],
        };

        assert_eq!(selected_text(&selection), "world\nsecond");
        let highlighted = highlighted_selection(&selection);
        assert_eq!(markdown::strip_ansi(&highlighted[0]), "hello world   ");
        assert!(highlighted[0].contains("\x1b[7m"));
    }

    #[test]
    fn clipboard_payload_uses_standard_base64() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode("copy me".as_bytes()), "Y29weSBtZQ==");
    }

    #[test]
    fn picker_is_bounded_and_highlights_selection() {
        let picker = Picker {
            title: "Choose model".into(),
            hint: "Enter select".into(),
            selected: 1,
            items: vec![
                PickerItem {
                    label: "First".into(),
                    description: "provider:first".into(),
                    action: PickerAction::SwitchModel("provider:first".into()),
                },
                PickerItem {
                    label: "Second".into(),
                    description: "provider:second".into(),
                    action: PickerAction::SwitchModel("provider:second".into()),
                },
            ],
            editing: None,
        };
        let rendered = render_picker(&picker, &Editor::default(), 50, 10);
        assert_eq!(rendered.len(), 10);
        assert!(
            rendered
                .iter()
                .all(|line| markdown::visible_width(line) == 50)
        );
        assert!(
            rendered
                .iter()
                .any(|line| { line.contains("Second") && line.contains("\x1b[7m") })
        );
    }

    #[test]
    fn accent_picker_selects_the_current_shared_color() {
        let blue = UiColor::new(117, 169, 255);
        let picker = color_picker(blue);

        assert_eq!(picker.title, "Accent color");
        assert!(matches!(
            picker.items[picker.selected].action,
            PickerAction::SetAccentColor(color) if color == blue
        ));
        assert!(picker.items.iter().any(|item| item.label == "Custom RGB…"));
    }

    #[test]
    fn editable_setting_stays_in_the_picker_and_submits_without_a_slash_command() {
        let mut state = ViewState {
            transcript: Transcript::from_messages(&[]),
            tools_expanded: false,
            model: "test".into(),
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            copy_toast_ticks: 0,
            context_tokens: 0,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            pending_actions: std::collections::VecDeque::new(),
            completions: Vec::new(),
            completion_index: 0,
            picker: Some(Picker {
                title: "Settings".into(),
                hint: "Enter change".into(),
                selected: 0,
                items: vec![PickerItem {
                    label: "Max output tokens".into(),
                    description: "8192".into(),
                    action: PickerAction::EditSetting {
                        key: "max_tokens".into(),
                        initial: "8192".into(),
                    },
                }],
                editing: None,
            }),
        };
        let mut editor = Editor::default();

        assert!(take_picker_action(&mut state, &mut editor, Key::Enter).is_none());
        assert!(picker_is_editing(&state));
        assert_eq!(editor.text(), "8192");

        editor.clear();
        editor.paste("16384");
        let action = take_picker_action(&mut state, &mut editor, Key::Enter);
        assert!(matches!(
            action,
            Some(PickerAction::ApplySetting {
                argument,
                selected: 0
            }) if argument == "max_tokens 16384"
        ));
        assert!(state.picker.is_none());
    }

    #[test]
    fn settings_and_model_pickers_are_recognized_during_an_active_turn() {
        assert_eq!(busy_command(" /settings "), Some(BusyCommand::Settings));
        assert_eq!(busy_command("/model"), Some(BusyCommand::Model));
        assert_eq!(
            busy_command("/unqueue 2"),
            Some(BusyCommand::Unqueue("2".into()))
        );
        assert_eq!(busy_command("/settings max_tokens 1"), None);
        assert_eq!(busy_command("hello"), None);
    }

    #[test]
    fn escape_and_ctrl_c_cancel_an_active_turn() {
        assert!(is_cancel_key(Key::Escape));
        assert!(is_cancel_key(Key::Ctrl('c')));
        assert!(!is_cancel_key(Key::Enter));
    }

    #[test]
    fn enter_submits_the_only_matching_command_completion() {
        let mut state = ViewState {
            transcript: Transcript::from_messages(&[]),
            tools_expanded: false,
            model: "test".into(),
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            copy_toast_ticks: 0,
            context_tokens: 0,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            pending_actions: std::collections::VecDeque::new(),
            completions: vec![Completion {
                command: "/quit".into(),
                description: "Exit Yawl".into(),
            }],
            completion_index: 0,
            picker: None,
        };
        let mut editor = Editor::default();
        editor.paste("/qui");

        assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
        assert_eq!(
            editor.handle_key(Key::Enter),
            EditAction::Submit("/quit ".into())
        );
    }

    #[test]
    fn queue_picker_removes_a_selected_message_and_keeps_the_rest() {
        let mut state = ViewState {
            transcript: Transcript::from_messages(&[]),
            tools_expanded: false,
            model: "test".into(),
            reasoning_effort: None,
            hide_reasoning: false,
            accent_color: UiColor::WHITE,
            copy_toast_ticks: 0,
            context_tokens: 0,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: ["first".into(), "second".into()].into(),
            pending_actions: std::collections::VecDeque::new(),
            completions: Vec::new(),
            completion_index: 0,
            picker: None,
        };
        open_queue_picker(&mut state);
        let mut editor = Editor::default();

        let action = take_picker_action(&mut state, &mut editor, Key::Enter);
        let remaining = action.and_then(|action| handle_queue_picker_action(&mut state, action));

        assert!(remaining.is_none());
        assert_eq!(
            state.queued_inputs,
            std::collections::VecDeque::from(["second".into()])
        );
        assert!(state.picker.is_some());
        assert_eq!(state.activity, "removed queued message 1");
    }

    #[test]
    fn queued_message_has_a_visible_waiting_label() {
        let rendered = render_queued_panel("follow up", 2, 50);
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("Queued 2 · waiting for the active response"));
        assert!(plain.contains("follow up"));
    }

    #[test]
    fn tool_entries_are_compact_and_visually_separated() {
        let output = (1..=30)
            .map(|line| format!("output line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let entries = vec![Entry::Tool {
            name: "shell".into(),
            args: r#"{"command":"cargo test --all-targets"}"#.into(),
            output,
            is_error: false,
            running: false,
        }];

        let rendered = render_entries(&entries, 80, false, false);
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(rendered.len() <= 16, "tool used {} lines", rendered.len());
        assert!(plain.contains("$ cargo test --all-targets"));
        assert!(plain.contains("lines, Ctrl+O to expand"));
        assert!(rendered.iter().any(|line| line.contains("\x1b[48;")));
    }

    #[test]
    fn reasoning_summary_is_one_line_and_full_reasoning_is_not() {
        let summary = Entry::Reasoning {
            kind: ReasoningKind::Summary,
            content: "Inspecting\n  the request".into(),
        };
        let full = Entry::Reasoning {
            kind: ReasoningKind::Full,
            content: "First step\n\nSecond step".into(),
        };

        let summary_lines = render_entries(&[summary], 80, false, false);
        let full_lines = render_entries(&[full], 80, false, false);

        assert_eq!(summary_lines.len(), 2);
        let summary_text = markdown::strip_ansi(&summary_lines[0]);
        assert!(summary_text.contains("Inspecting the request"));
        assert!(!summary_text.contains("Reasoning"));
        assert!(full_lines.len() > summary_lines.len());
        assert!(!markdown::strip_ansi(&full_lines.join("\n")).contains("Reasoning"));
    }

    #[test]
    fn hidden_reasoning_is_removed_from_the_transcript() {
        let reasoning = Entry::Reasoning {
            kind: ReasoningKind::Full,
            content: "private thought".into(),
        };

        assert!(render_entries(&[reasoning], 80, false, true).is_empty());
    }

    #[test]
    fn reasoning_has_one_blank_line_on_each_side() {
        let entries = vec![
            Entry::Assistant("Answer\n\n".into()),
            Entry::Reasoning {
                kind: ReasoningKind::Full,
                content: "\nThinking\n\n".into(),
            },
            Entry::Tool {
                name: "shell".into(),
                args: r#"{"command":"true"}"#.into(),
                output: String::new(),
                is_error: false,
                running: false,
            },
        ];

        let rendered = render_entries(&entries, 40, false, false);
        let plain = rendered
            .iter()
            .map(|line| markdown::strip_ansi(line).trim_end().to_string())
            .collect::<Vec<_>>();

        assert_eq!(plain, ["Answer", "", "Thinking", "", "", "$ true", "", ""]);
    }
}
