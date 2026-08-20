//! Full-screen terminal UI built directly on termios and ANSI escape
//! sequences. The terminal remains responsive while the blocking agent loop
//! runs on a scoped worker thread.

pub mod events;
pub mod highlight;
pub mod input;
pub mod markdown;
mod tool_view;

use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::{Agent, TurnEvent};
use crate::error::Error;
use crate::provider::{Message, ReasoningKind, Role};

use self::events::{Event, EventReader, Key};
use self::input::{EditAction, Editor};

const USER_BACKGROUND: &str = "\x1b[48;2;52;53;64m";
const USER_TEXT: &str = "\x1b[38;2;208;208;214m";

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
  /help                show this help
  /quit                leave Yawl

Input
  Enter submits. Shift+Enter or Alt+Enter inserts a newline.
  Type / for commands; Up/Down select and Tab completes a menu item.
  Outside the menu, Up and Down browse input history. Ctrl+U, Ctrl+K, and Ctrl+W edit.
  Ctrl+O expands or collapses tool output. Ctrl+C aborts the active turn.
  Mouse wheel and PageUp/PageDown scroll.
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
    ToggleHideReasoning,
    ResumeSession(String),
    EditSetting(String),
    ToggleAutoCompact,
    Reload,
    ShowSettings,
}

struct PickerItem {
    label: String,
    description: String,
    action: PickerAction,
}

struct Picker {
    title: String,
    hint: String,
    items: Vec<PickerItem>,
    selected: usize,
}

enum Entry {
    User(String),
    Assistant(String),
    Reasoning {
        kind: ReasoningKind,
        content: String,
    },
    Tool {
        name: String,
        args: String,
        output: String,
        is_error: bool,
        running: bool,
    },
    Notice(String),
}

struct ViewState {
    entries: Vec<Entry>,
    streaming_entries_start: Option<usize>,
    streaming_assistant: Option<usize>,
    streaming_reasoning: Option<(ReasoningKind, usize)>,
    running_tool: Option<usize>,
    tools_expanded: bool,
    model: String,
    reasoning_effort: Option<String>,
    hide_reasoning: bool,
    context_tokens: u64,
    context_window: u64,
    activity: String,
    scroll_offset: usize,
    queued_inputs: std::collections::VecDeque<String>,
    completions: Vec<Completion>,
    completion_index: usize,
    picker: Option<Picker>,
}

impl ViewState {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            entries: entries_from_messages(&agent.messages),
            streaming_entries_start: None,
            streaming_assistant: None,
            streaming_reasoning: None,
            running_tool: None,
            tools_expanded: false,
            model: agent.model.clone(),
            reasoning_effort: agent.config.reasoning_effort.clone(),
            hide_reasoning: agent.config.hide_reasoning,
            context_tokens: agent.context_tokens,
            context_window: agent.context_window(),
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
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
        self.entries.push(Entry::Notice(text.into()));
        self.scroll_offset = 0;
    }

    fn apply(&mut self, update: Update) {
        let follow_bottom = self.scroll_offset == 0;
        match update {
            Update::TextDelta(text) => {
                self.streaming_entries_start
                    .get_or_insert(self.entries.len());
                self.streaming_reasoning = None;
                let index = match self.streaming_assistant {
                    Some(index) => index,
                    None => {
                        self.entries.push(Entry::Assistant(String::new()));
                        let index = self.entries.len() - 1;
                        self.streaming_assistant = Some(index);
                        index
                    }
                };
                if let Some(Entry::Assistant(content)) = self.entries.get_mut(index) {
                    content.push_str(&text);
                }
                self.activity = "responding".into();
            }
            Update::ReasoningDelta { kind, text } => {
                self.streaming_entries_start
                    .get_or_insert(self.entries.len());
                self.streaming_assistant = None;
                let index = match self.streaming_reasoning {
                    Some((current_kind, index)) if current_kind == kind => index,
                    _ => {
                        self.entries.push(Entry::Reasoning {
                            kind,
                            content: String::new(),
                        });
                        let index = self.entries.len() - 1;
                        self.streaming_reasoning = Some((kind, index));
                        index
                    }
                };
                if let Some(Entry::Reasoning { content, .. }) = self.entries.get_mut(index) {
                    content.push_str(&text);
                }
                self.activity = if self.hide_reasoning {
                    "responding".into()
                } else {
                    "reasoning".into()
                };
            }
            Update::RetryReset => {
                if let Some(start) = self.streaming_entries_start {
                    self.entries.truncate(start);
                }
                self.streaming_assistant = None;
                self.streaming_reasoning = None;
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
            Update::AssistantDone => {
                self.streaming_entries_start = None;
                self.streaming_assistant = None;
                self.streaming_reasoning = None;
                self.activity.clear();
            }
            Update::ToolStart { name, args } => {
                self.entries.push(Entry::Tool {
                    name,
                    args,
                    output: String::new(),
                    is_error: false,
                    running: true,
                });
                self.running_tool = Some(self.entries.len() - 1);
                self.activity = "running tool".into();
            }
            Update::ToolEnd {
                name,
                output,
                is_error,
            } => {
                let index = self.running_tool.take();
                if let Some(Entry::Tool {
                    name: entry_name,
                    output: entry_output,
                    is_error: entry_error,
                    running,
                    ..
                }) = index.and_then(|index| self.entries.get_mut(index))
                {
                    *entry_name = name;
                    *entry_output = output;
                    *entry_error = is_error;
                    *running = false;
                }
                self.activity.clear();
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
    TextDelta(String),
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    AssistantDone,
    ToolStart {
        name: String,
        args: String,
    },
    ToolEnd {
        name: String,
        output: String,
        is_error: bool,
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
            TurnEvent::TextDelta(text) => Self::TextDelta(text.to_string()),
            TurnEvent::ReasoningDelta { kind, text } => Self::ReasoningDelta {
                kind,
                text: text.to_string(),
            },
            TurnEvent::RetryReset => Self::RetryReset,
            TurnEvent::Retrying {
                attempt,
                delay_ms,
                error,
            } => Self::Retrying {
                attempt,
                delay_ms,
                error,
            },
            TurnEvent::AssistantDone => Self::AssistantDone,
            TurnEvent::ToolStart { name, args } => Self::ToolStart {
                name: name.to_string(),
                args: args.to_string(),
            },
            TurnEvent::ToolEnd {
                name,
                output,
                is_error,
            } => Self::ToolEnd {
                name: name.to_string(),
                output: output.to_string(),
                is_error,
            },
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
    if state.entries.is_empty() {
        state.notice("Yawl is ready. Type /help for commands.");
    }
    terminal.draw(&mut state, &editor)?;

    loop {
        if let Some(input) = state.queued_inputs.pop_front() {
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
                Event::Key(key) => handle_picker_key(agent, &mut state, &mut editor, key),
                Event::Tick | Event::MouseScroll(_) | Event::Paste(_) => {}
            }
            terminal.draw(&mut state, &editor)?;
            continue;
        }
        match event {
            Event::Tick => {
                if crate::interrupted() {
                    crate::set_interrupted(false);
                    if !editor.is_empty() {
                        editor.clear();
                    }
                    state.activity = "input cleared".into();
                }
            }
            Event::MouseScroll(amount) => scroll(&mut state, amount),
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
        let skills = crate::skills::scan(&agent.config);
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
                agent.model = argument.to_string();
                state.model.clone_from(&agent.model);
                state.context_window = agent.context_window();
                state.context_tokens = 0;
                if agent.model.starts_with("openai-codex:") {
                    open_reasoning_picker(agent, state, false);
                } else {
                    state.notice(format!("Switched to {}.", agent.model));
                }
            }
            "settings" if argument.is_empty() => open_settings_picker(agent, state),
            "settings" => {
                settings(agent, argument, state);
                state.refresh_completions(agent);
            }
            name if is_new_session_command(name) => match agent.reset() {
                Ok(()) => {
                    let queued_inputs = std::mem::take(&mut state.queued_inputs);
                    *state = ViewState::from_agent(agent);
                    state.queued_inputs = queued_inputs;
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
    state.entries.push(Entry::User(displayed_input));
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
        crate::skills::scan(&agent.config)
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
        _ => false,
    }
}

fn show_skills(agent: &Agent, state: &mut ViewState) {
    let skills = crate::skills::scan(&agent.config);
    let mut text = String::from("Skill directories\n\n");
    for dir in &agent.config.skill_dirs {
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

fn expand_user_path(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return std::env::var_os("HOME").map_or_else(|| path.into(), Into::into);
    }
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(relative);
    }
    path.into()
}

fn open_model_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    let selected_model = if save {
        agent.config.model.as_deref().unwrap_or(&agent.model)
    } else {
        &agent.model
    };
    let mut models = agent.config.available_models();
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
        action: PickerAction::EditSetting(if save {
            "/settings model ".into()
        } else {
            "/model ".into()
        }),
    });
    let selected = items
        .iter()
        .position(|item| item.description == selected_model)
        .unwrap_or(0);
    state.picker = Some(Picker {
        title: if save {
            "Default model".into()
        } else {
            "Choose model".into()
        },
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
    });
}

fn open_settings_picker(agent: &Agent, state: &mut ViewState) {
    let on_off = if agent.config.auto_compact {
        "On"
    } else {
        "Off"
    };
    let reasoning_visibility = if agent.config.hide_reasoning {
        "Hidden"
    } else {
        "Visible"
    };
    state.picker = Some(Picker {
        title: "Settings".into(),
        hint: "↑/↓ move  Enter change  Esc close".into(),
        selected: 0,
        items: vec![
            PickerItem {
                label: "Default model".into(),
                description: agent
                    .config
                    .model
                    .clone()
                    .unwrap_or_else(|| agent.model.clone()),
                action: PickerAction::OpenModels { save: true },
            },
            PickerItem {
                label: "Max output tokens".into(),
                description: agent.config.max_tokens.to_string(),
                action: PickerAction::EditSetting("/settings max_tokens ".into()),
            },
            PickerItem {
                label: "Codex reasoning effort".into(),
                description: agent
                    .config
                    .reasoning_effort
                    .clone()
                    .unwrap_or_else(|| "provider default".into()),
                action: if agent.model.starts_with("openai-codex:") {
                    PickerAction::OpenReasoning { save: true }
                } else {
                    PickerAction::EditSetting("/settings reasoning_effort ".into())
                },
            },
            PickerItem {
                label: "Reasoning display".into(),
                description: format!("{reasoning_visibility} · Enter to toggle"),
                action: PickerAction::ToggleHideReasoning,
            },
            PickerItem {
                label: "Automatic compaction".into(),
                description: format!("{on_off} · Enter to toggle"),
                action: PickerAction::ToggleAutoCompact,
            },
            PickerItem {
                label: "Compaction threshold".into(),
                description: format!("{:.0}%", agent.config.compact_threshold * 100.0),
                action: PickerAction::EditSetting("/settings compact_threshold ".into()),
            },
            PickerItem {
                label: "Current model context window".into(),
                description: agent.context_window().to_string(),
                action: PickerAction::EditSetting("/settings context_window ".into()),
            },
            PickerItem {
                label: "Skill directories".into(),
                description: format!(
                    "{} configured · add or remove",
                    agent.config.skill_dirs.len()
                ),
                action: PickerAction::EditSetting("/settings skills ".into()),
            },
            PickerItem {
                label: "OpenAI-compatible provider".into(),
                description: "Add or update a provider".into(),
                action: PickerAction::EditSetting("/settings provider ".into()),
            },
            PickerItem {
                label: "OpenAI endpoint".into(),
                description: agent.config.openai_base_url.clone(),
                action: PickerAction::EditSetting("/settings openai_base_url ".into()),
            },
            PickerItem {
                label: "Anthropic endpoint".into(),
                description: agent.config.anthropic_base_url.clone(),
                action: PickerAction::EditSetting("/settings anthropic_base_url ".into()),
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
    });
}

fn open_reasoning_picker(agent: &Agent, state: &mut ViewState, save: bool) {
    let model = agent
        .model
        .strip_prefix("openai-codex:")
        .unwrap_or(&agent.model);
    let current = agent.config.reasoning_effort.as_deref();
    let mut items = vec![PickerItem {
        label: "Provider default".into(),
        description: "Do not request a specific effort".into(),
        action: PickerAction::SetReasoning { effort: None, save },
    }];
    items.extend(
        crate::provider::codex::reasoning_efforts(model)
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
    state.picker = Some(Picker {
        title: format!("Reasoning · {model}"),
        hint: "↑/↓ move  Enter select  Esc cancel".into(),
        items,
        selected,
    });
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

fn handle_picker_key(agent: &mut Agent, state: &mut ViewState, editor: &mut Editor, key: Key) {
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
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
            state.picker = None;
            if let Some(action) = action {
                activate_picker_action(agent, state, editor, action);
            }
        }
        _ => {}
    }
}

fn activate_picker_action(
    agent: &mut Agent,
    state: &mut ViewState,
    editor: &mut Editor,
    action: PickerAction,
) {
    match action {
        PickerAction::SwitchModel(model) => {
            agent.model = model;
            state.model.clone_from(&agent.model);
            state.context_window = agent.context_window();
            state.context_tokens = 0;
            if agent.model.starts_with("openai-codex:") {
                open_reasoning_picker(agent, state, false);
            } else {
                state.notice(format!("Switched to {}.", agent.model));
            }
        }
        PickerAction::SaveModel(model) => {
            settings(agent, &format!("model {model}"), state);
            if agent.model.starts_with("openai-codex:") {
                open_reasoning_picker(agent, state, true);
            }
        }
        PickerAction::OpenModels { save } => open_model_picker(agent, state, save),
        PickerAction::OpenReasoning { save } => open_reasoning_picker(agent, state, save),
        PickerAction::SetReasoning { effort, save } => {
            if save {
                let value = effort.as_deref().unwrap_or("default");
                settings(agent, &format!("reasoning_effort {value}"), state);
            } else {
                agent.config.reasoning_effort.clone_from(&effort);
                state.reasoning_effort = effort;
                let label = state
                    .reasoning_effort
                    .as_deref()
                    .unwrap_or("provider default");
                state.notice(format!("Using {} with {label} reasoning.", agent.model));
            }
        }
        PickerAction::ToggleHideReasoning => {
            let value = if agent.config.hide_reasoning {
                "off"
            } else {
                "on"
            };
            settings(agent, &format!("hide_reasoning {value}"), state);
        }
        PickerAction::ResumeSession(id) => load_session(agent, &id, state),
        PickerAction::EditSetting(command) => editor.paste(&command),
        PickerAction::ToggleAutoCompact => {
            let value = if agent.config.auto_compact {
                "off"
            } else {
                "on"
            };
            settings(agent, &format!("auto_compact {value}"), state);
        }
        PickerAction::Reload => settings(agent, "reload", state),
        PickerAction::ShowSettings => show_settings(agent, state),
    }
    state.refresh_completions(agent);
}

fn settings(agent: &mut Agent, argument: &str, state: &mut ViewState) {
    if argument.is_empty() {
        show_settings(agent, state);
        return;
    }

    let mut parts = argument.split_whitespace();
    let key = parts.next().unwrap_or_default();
    let result = match key {
        "reload" => {
            if parts.next().is_some() {
                Err(Error::Config("usage: /settings reload".into()))
            } else {
                reload_config(agent, state, true)
            }
        }
        "model" => one_value(&mut parts, "usage: /settings model MODEL").and_then(|model| {
            agent
                .config
                .save_global_setting("model", serde_json::json!(model))?;
            reload_config(agent, state, false)
        }),
        "max_tokens" => {
            one_value(&mut parts, "usage: /settings max_tokens NUMBER").and_then(|value| {
                let tokens = value
                    .parse::<u32>()
                    .map_err(|_| Error::Config("max_tokens must be a positive integer".into()))?;
                if tokens == 0 {
                    return Err(Error::Config(
                        "max_tokens must be a positive integer".into(),
                    ));
                }
                agent
                    .config
                    .save_global_setting("max_tokens", serde_json::json!(tokens))?;
                reload_config(agent, state, true)
            })
        }
        "reasoning_effort" => one_value(
            &mut parts,
            "usage: /settings reasoning_effort default|minimal|low|medium|high|xhigh|max",
        )
        .and_then(|value| {
            if !matches!(
                value,
                "default" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            ) {
                return Err(Error::Config("unsupported reasoning effort".into()));
            }
            agent
                .config
                .save_global_setting("reasoning_effort", serde_json::json!(value))?;
            reload_config(agent, state, true)
        }),
        "hide_reasoning" => one_value(&mut parts, "usage: /settings hide_reasoning on|off")
            .and_then(|value| {
                let hidden = parse_on_off(value)?;
                agent
                    .config
                    .save_global_setting("hide_reasoning", serde_json::json!(hidden))?;
                reload_config(agent, state, true)
            }),
        "auto_compact" => {
            one_value(&mut parts, "usage: /settings auto_compact on|off").and_then(|value| {
                let enabled = parse_on_off(value)?;
                agent
                    .config
                    .save_global_setting("auto_compact", serde_json::json!(enabled))?;
                reload_config(agent, state, true)
            })
        }
        "compact_threshold" => one_value(
            &mut parts,
            "usage: /settings compact_threshold FRACTION|PERCENT%",
        )
        .and_then(|value| {
            let threshold = parse_threshold(value)?;
            agent
                .config
                .save_global_setting("compact_threshold", serde_json::json!(threshold))?;
            reload_config(agent, state, true)
        }),
        "context_window" => one_value(&mut parts, "usage: /settings context_window TOKENS")
            .and_then(|value| {
                let window = value.parse::<u64>().map_err(|_| {
                    Error::Config("context_window must be a positive integer".into())
                })?;
                if window == 0 {
                    return Err(Error::Config(
                        "context_window must be a positive integer".into(),
                    ));
                }
                agent
                    .config
                    .save_global_context_window(&agent.model, window)?;
                reload_config(agent, state, true)
            }),
        "skills" => {
            let action = parts.next();
            let path = parts.next();
            if !matches!(action, Some("add" | "remove")) || path.is_none() || parts.next().is_some()
            {
                Err(Error::Config(
                    "usage: /settings skills add|remove DIRECTORY".into(),
                ))
            } else {
                let path = expand_user_path(path.unwrap_or_default());
                let mut dirs = agent.config.skill_dirs.clone();
                if action == Some("add") {
                    if !dirs.contains(&path) {
                        dirs.push(path);
                    }
                } else if let Some(index) = dirs.iter().position(|dir| dir == &path) {
                    dirs.remove(index);
                } else {
                    return state.notice(format!(
                        "Skill directory `{}` is not configured.",
                        path.display()
                    ));
                }
                agent
                    .config
                    .save_global_skill_dirs(&dirs)
                    .and_then(|()| reload_config(agent, state, true))
            }
        }
        "anthropic_base_url" | "openai_base_url" => {
            one_value(&mut parts, "usage: /settings openai_base_url URL").and_then(|url| {
                validate_http_url(url)?;
                agent
                    .config
                    .save_global_setting(key, serde_json::json!(url))?;
                reload_config(agent, state, true)
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
                agent
                    .config
                    .save_global_provider(
                        name.unwrap_or_default(),
                        url.unwrap_or_default(),
                        api_key,
                    )
                    .and_then(|()| reload_config(agent, state, true))
            }
        }
        _ => Err(Error::Config(format!(
            "unknown setting '{key}'; run /settings to list settings"
        ))),
    };

    match result {
        Ok(()) => {
            state.context_window = agent.context_window();
            state.notice(format!(
                "Saved to `{}` and applied.",
                agent.config.global_config_path().display()
            ));
        }
        Err(error) => state.notice(format!("Could not change setting: {error}")),
    }
}

fn show_settings(agent: &Agent, state: &mut ViewState) {
    let mut providers = agent.config.providers.iter().collect::<Vec<_>>();
    providers.sort_by_key(|(name, _)| name.as_str());
    let mut text = format!(
        "Settings\n\n- model: `{}`\n- max_tokens: `{}`\n- reasoning_effort: `{}`\n- hide_reasoning: `{}`\n- auto_compact: `{}`\n- compact_threshold: `{:.0}%`\n- context_window for current model: `{}`\n- anthropic_base_url: `{}`\n- openai_base_url: `{}`\n\nSkill directories\n\n",
        agent.model,
        agent.config.max_tokens,
        agent
            .config
            .reasoning_effort
            .as_deref()
            .unwrap_or("provider default"),
        agent.config.hide_reasoning,
        if agent.config.auto_compact {
            "on"
        } else {
            "off"
        },
        agent.config.compact_threshold * 100.0,
        agent.context_window(),
        agent.config.anthropic_base_url,
        agent.config.openai_base_url,
    );
    for dir in &agent.config.skill_dirs {
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
        "\nChanges are written to `{}`. Project settings in `./.yawl/config.json` override them.\n\nCommands\n\n- `/settings model MODEL`\n- `/settings max_tokens NUMBER`\n- `/settings reasoning_effort default|minimal|low|medium|high|xhigh|max`\n- `/settings hide_reasoning on|off`\n- `/settings auto_compact on|off`\n- `/settings compact_threshold 85%`\n- `/settings context_window TOKENS`\n- `/settings skills add|remove DIRECTORY`\n- `/settings provider NAME BASE_URL [API_KEY|-]`\n- `/settings openai_base_url URL`\n- `/settings anthropic_base_url URL`\n- `/settings reload`\n\nUse an environment reference such as `$OMLX_API_KEY` instead of putting a secret directly in terminal history. Pass `-` as the provider key to remove a saved key.",
        agent.config.global_config_path().display()
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

fn parse_on_off(value: &str) -> Result<bool, Error> {
    match value {
        "on" | "true" => Ok(true),
        "off" | "false" => Ok(false),
        _ => Err(Error::Config("expected on or off".into())),
    }
}

fn parse_threshold(value: &str) -> Result<f64, Error> {
    let threshold = if let Some(percent) = value.strip_suffix('%') {
        percent
            .parse::<f64>()
            .map_err(|_| Error::Config("invalid compaction percentage".into()))?
            / 100.0
    } else {
        value
            .parse::<f64>()
            .map_err(|_| Error::Config("invalid compaction threshold".into()))?
    };
    if !threshold.is_finite() || !(0.1..=0.99).contains(&threshold) {
        return Err(Error::Config(
            "compact_threshold must be between 0.1 and 0.99".into(),
        ));
    }
    Ok(threshold)
}

fn validate_http_url(url: &str) -> Result<(), Error> {
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(Error::Config(
            "provider URL must start with http:// or https://".into(),
        ))
    }
}

fn reload_config(agent: &mut Agent, state: &mut ViewState, keep_model: bool) -> Result<(), Error> {
    let current_model = agent.model.clone();
    agent.config = crate::config::Config::load()?;
    if keep_model {
        agent.model = current_model;
    } else {
        agent.model = agent
            .config
            .model
            .clone()
            .ok_or_else(|| Error::Config("no model configured".into()))?;
        agent.context_tokens = 0;
    }
    state.model.clone_from(&agent.model);
    state.reasoning_effort = agent.config.reasoning_effort.clone();
    state.hide_reasoning = agent.config.hide_reasoning;
    state.context_window = agent.context_window();
    Ok(())
}

fn open_resume_picker(agent: &Agent, state: &mut ViewState) {
    let sessions = match crate::session::list(&agent.config.sessions_dir()) {
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
    });
}

fn resume(agent: &mut Agent, selector: &str, state: &mut ViewState) {
    let sessions = match crate::session::list(&agent.config.sessions_dir()) {
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
            *state = ViewState::from_agent(agent);
            state.queued_inputs = queued_inputs;
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
            updates_rx,
            done_rx,
            worker_thread,
            state,
            editor,
            terminal,
            events,
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
            updates_rx,
            done_rx,
            worker_thread,
            state,
            editor,
            terminal,
            events,
        )
    })
}

fn pump_events<R: Read, T>(
    updates: Receiver<Update>,
    done: Receiver<Result<T, Error>>,
    worker_thread: usize,
    state: &mut ViewState,
    editor: &mut Editor,
    terminal: &mut Terminal,
    events: &mut EventReader<R>,
) -> Result<T, Error> {
    loop {
        while let Ok(update) = updates.try_recv() {
            state.apply(update);
        }
        match done.try_recv() {
            Ok(result) => {
                while let Ok(update) = updates.try_recv() {
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
        match events.read_event()? {
            Event::Tick => {
                if crate::interrupted() {
                    state.activity = "canceling turn".into();
                }
            }
            Event::MouseScroll(amount) => scroll(state, amount),
            Event::Paste(text) => editor.paste(&text),
            Event::Key(Key::Ctrl('c')) => {
                crate::set_interrupted(true);
                interrupt_thread(worker_thread);
                state.activity = "canceling turn".into();
            }
            Event::Key(Key::Ctrl('l')) => terminal.invalidate(),
            Event::Key(Key::Ctrl('o')) => toggle_tool_expansion(state),
            Event::Key(Key::PageUp) => scroll(state, 10),
            Event::Key(Key::PageDown) => scroll(state, -10),
            Event::Key(key) => {
                if handle_completion_key(state, editor, key) {
                    // Keep accepting and completing input while the agent runs.
                } else if let EditAction::Submit(input) = editor.handle_key(key) {
                    state.queued_inputs.push_back(input);
                }
            }
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

fn entries_from_messages(messages: &[Message]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut pending_tools = std::collections::VecDeque::new();
    for message in messages {
        match message.role {
            Role::User if message.content.starts_with("[conversation summary]") => {
                entries.push(Entry::Notice(message.content.clone()));
            }
            Role::User => entries.push(Entry::User(message.content.clone())),
            Role::Assistant => {
                for reasoning in &message.reasoning {
                    if !reasoning.content.is_empty() {
                        entries.push(Entry::Reasoning {
                            kind: reasoning.kind,
                            content: reasoning.content.clone(),
                        });
                    }
                }
                if !message.content.is_empty() {
                    entries.push(Entry::Assistant(message.content.clone()));
                }
                for call in &message.tool_calls {
                    entries.push(Entry::Tool {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        output: String::new(),
                        is_error: false,
                        running: false,
                    });
                    pending_tools.push_back((call.id.as_str(), entries.len() - 1));
                }
            }
            Role::Tool => {
                let pending_position = message.tool_call_id.as_deref().and_then(|id| {
                    pending_tools
                        .iter()
                        .position(|(pending_id, _)| *pending_id == id)
                });
                let pending_index = pending_position
                    .and_then(|position| pending_tools.remove(position))
                    .map(|(_, index)| index);
                if let Some(Entry::Tool {
                    name,
                    output,
                    is_error,
                    ..
                }) = pending_index.and_then(|index| entries.get_mut(index))
                {
                    if let Some(tool_name) = &message.tool_name {
                        name.clone_from(tool_name);
                    }
                    output.clone_from(&message.content);
                    *is_error = message.is_error;
                } else {
                    entries.push(Entry::Tool {
                        name: message.tool_name.clone().unwrap_or_else(|| "tool".into()),
                        args: String::new(),
                        output: message.content.clone(),
                        is_error: message.is_error,
                        running: false,
                    });
                }
            }
        }
    }
    entries
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

struct Terminal {
    original: libc::termios,
    stdout: io::Stdout,
    active: bool,
    last_frame: Vec<String>,
    last_size: (u16, u16),
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
            last_size: (0, 0),
        };
        terminal
            .stdout
            .write_all(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[>1u")?;
        terminal.stdout.flush()?;
        Ok(terminal)
    }

    fn invalidate(&mut self) {
        self.last_frame.clear();
        self.last_size = (0, 0);
    }

    fn draw(&mut self, state: &mut ViewState, editor: &Editor) -> Result<(), Error> {
        let (columns, rows) = terminal_size();
        let (frame, cursor) = build_frame(state, editor, usize::from(columns), usize::from(rows));
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
        write!(self.stdout, "\x1b[{};{}H\x1b[?25h", cursor.0, cursor.1)?;
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
        let _ = self
            .stdout
            .write_all(b"\x1b[<u\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = self.stdout.flush();
        // SAFETY: `original` came from a successful `tcgetattr` call for
        // STDIN_FILENO and remains initialized for the life of this guard.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.original);
        }
        self.active = false;
    }
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

fn render_picker(picker: &Picker, columns: usize, height: usize) -> Vec<String> {
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
        let text = format!(" {marker} {}  ·  {}", item.label, item.description);
        if absolute == picker.selected {
            panel.push(boxed(&format!(
                "\x1b[7m{}\x1b[0m",
                markdown::fit_width(&text, inner)
            )));
        } else {
            panel.push(boxed(&text));
        }
    }
    panel.push(boxed(&format!(" \x1b[2m{}\x1b[0m", picker.hint)));
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
    let transcript = render_entries(
        &state.entries,
        columns,
        state.tools_expanded,
        state.hide_reasoning,
    );
    let max_scroll = transcript.len().saturating_sub(transcript_height);
    state.scroll_offset = state.scroll_offset.min(max_scroll);
    let end = transcript.len().saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(transcript_height);
    let visible = &transcript[start..end];

    let mut frame = Vec::with_capacity(rows);
    if let Some(picker) = &state.picker {
        frame.extend(render_picker(picker, columns, transcript_height));
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
    frame.push(format!("┌{}┐", "─".repeat(inner_width)));
    for line in input_lines {
        frame.push(format!("│{}│", markdown::fit_width(line, inner_width)));
    }
    frame.push(format!("└{}┘", "─".repeat(inner_width)));

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
    frame.push(format!(
        "\x1b[7m{}\x1b[0m",
        markdown::fit_width(&status, columns)
    ));

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
            entries: vec![Entry::Assistant("hello".into())],
            streaming_entries_start: None,
            streaming_assistant: None,
            streaming_reasoning: None,
            running_tool: None,
            tools_expanded: false,
            model: "test".into(),
            reasoning_effort: None,
            hide_reasoning: false,
            context_tokens: 12,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            completions: Vec::new(),
            completion_index: 0,
            picker: None,
        };
        let editor = Editor::default();
        let (frame, cursor) = build_frame(&mut state, &editor, 40, 12);
        assert_eq!(frame.len(), 12);
        assert!(markdown::strip_ansi(frame.last().unwrap()).contains("test"));
        assert_eq!(cursor.0, 10);
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
        };
        let rendered = render_picker(&picker, 50, 10);
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
    fn loaded_messages_become_transcript_entries() {
        let mut assistant = Message::assistant(
            "hello".into(),
            vec![crate::provider::ToolCall {
                id: "id".into(),
                name: "shell".into(),
                arguments: r#"{"command":"pwd"}"#.into(),
            }],
        );
        assistant.reasoning.push(crate::provider::Reasoning {
            kind: ReasoningKind::Summary,
            content: "Checking the directory".into(),
        });
        let messages = vec![
            Message::user("hi"),
            assistant,
            Message::tool_result("id", "shell", "ok".into(), false),
        ];
        let entries = entries_from_messages(&messages);
        assert_eq!(entries.len(), 4);
        let Entry::Tool { args, output, .. } = &entries[3] else {
            panic!("expected paired tool entry");
        };
        assert!(args.contains("pwd"));
        assert_eq!(output, "ok");
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

        assert_eq!(plain, ["Answer", "", "Thinking", "", "$ true", ""]);
    }
}
