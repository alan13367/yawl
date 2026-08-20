//! Full-screen terminal UI built directly on termios and ANSI escape
//! sequences. The terminal remains responsive while the blocking agent loop
//! runs on a scoped worker thread.

pub mod events;
pub mod highlight;
pub mod input;
pub mod markdown;

use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::agent::{Agent, TurnEvent};
use crate::error::Error;
use crate::provider::{Message, Role};

use self::events::{Event, EventReader, Key};
use self::input::{EditAction, Editor};

const HELP: &str = "\
Commands
  /model [MODEL]       show or switch the current model
  /clear               start a new session
  /compact             summarize older messages now
  /tools               list builtin and discovered tools
  /resume [ID|NUMBER]  list or resume saved sessions
  /help                show this help
  /quit                leave Yawl

Input
  Enter submits. Shift+Enter or Alt+Enter inserts a newline.
  Up and Down browse input history. Ctrl+U, Ctrl+K, and Ctrl+W edit.
  Mouse wheel and PageUp/PageDown scroll. Ctrl+C aborts the active turn.
";

enum Entry {
    User(String),
    Assistant(String),
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
    streaming_assistant: Option<usize>,
    running_tool: Option<usize>,
    model: String,
    context_tokens: u64,
    context_window: u64,
    activity: String,
    scroll_offset: usize,
    queued_inputs: std::collections::VecDeque<String>,
}

impl ViewState {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            entries: entries_from_messages(&agent.messages),
            streaming_assistant: None,
            running_tool: None,
            model: agent.model.clone(),
            context_tokens: agent.context_tokens,
            context_window: agent.context_window(),
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
        }
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry::Notice(text.into()));
        self.scroll_offset = 0;
    }

    fn apply(&mut self, update: Update) {
        let follow_bottom = self.scroll_offset == 0;
        match update {
            Update::TextDelta(text) => {
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
            Update::RetryReset => {
                if let Some(index) = self.streaming_assistant
                    && let Some(Entry::Assistant(content)) = self.entries.get_mut(index)
                {
                    content.clear();
                }
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
                self.streaming_assistant = None;
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
            Event::Key(key) => {
                if let EditAction::Submit(input) = editor.handle_key(key)
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
    if let Some(command) = command.strip_prefix('/') {
        let (name, argument) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(name, argument)| (name, argument.trim()));
        match name {
            "quit" | "q" => return Ok(true),
            "help" => state.notice(HELP),
            "model" if argument.is_empty() => {
                state.notice(format!("Current model: {}", agent.model));
            }
            "model" => {
                agent.model = argument.to_string();
                state.model.clone_from(&agent.model);
                state.context_window = agent.context_window();
                state.context_tokens = 0;
                state.notice(format!("Switched to {}.", agent.model));
            }
            "clear" => match agent.reset() {
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
            "resume" => resume(agent, argument, state),
            "" => {}
            other => state.notice(format!("Unknown command '/{other}'. Type /help.")),
        }
        return Ok(false);
    }

    state.entries.push(Entry::User(input.clone()));
    state.activity = "sending".into();
    state.scroll_offset = 0;
    terminal.draw(state, editor)?;
    match turn_interactive(agent, input, state, editor, terminal, events) {
        Ok(true) => {}
        Ok(false) | Err(Error::Interrupted) => state.notice("Turn interrupted."),
        Err(error) => state.notice(format!("Request failed: {error}")),
    }
    crate::set_interrupted(false);
    state.activity.clear();
    Ok(false)
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
            Event::Key(Key::PageUp) => scroll(state, 10),
            Event::Key(Key::PageDown) => scroll(state, -10),
            Event::Key(key) => {
                if let EditAction::Submit(input) = editor.handle_key(key) {
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

fn entries_from_messages(messages: &[Message]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for message in messages {
        match message.role {
            Role::User if message.content.starts_with("[conversation summary]") => {
                entries.push(Entry::Notice(message.content.clone()));
            }
            Role::User => entries.push(Entry::User(message.content.clone())),
            Role::Assistant => {
                if !message.content.is_empty() {
                    entries.push(Entry::Assistant(message.content.clone()));
                }
            }
            Role::Tool => entries.push(Entry::Tool {
                name: message.tool_name.clone().unwrap_or_else(|| "tool".into()),
                args: String::new(),
                output: message.content.clone(),
                is_error: message.is_error,
                running: false,
            }),
        }
    }
    entries
}

fn render_entries(entries: &[Entry], width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry {
            Entry::User(content) => {
                lines.push("\x1b[1;36mYou\x1b[0m".into());
                lines.extend(markdown::render(content, width));
            }
            Entry::Assistant(content) => {
                lines.push("\x1b[1;35mYawl\x1b[0m".into());
                lines.extend(markdown::render(content, width));
            }
            Entry::Tool {
                name,
                args,
                output,
                is_error,
                running,
            } => {
                let state = if *running {
                    "running"
                } else if *is_error {
                    "error"
                } else {
                    "done"
                };
                let color = if *is_error { 31 } else { 33 };
                lines.push(format!("\x1b[1;{color}mTool: {name} [{state}]\x1b[0m"));
                if !args.is_empty() {
                    let pretty = serde_json::from_str::<serde_json::Value>(args)
                        .ok()
                        .and_then(|value| serde_json::to_string_pretty(&value).ok())
                        .unwrap_or_else(|| args.clone());
                    lines.extend(markdown::render(&format!("```json\n{pretty}\n```"), width));
                }
                if !output.is_empty() {
                    lines.extend(markdown::render(output, width));
                }
            }
            Entry::Notice(content) => {
                lines.push("\x1b[1;33mYawl\x1b[0m".into());
                lines.extend(markdown::render(content, width));
            }
        }
        lines.push(String::new());
    }
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
    let transcript_height = rows.saturating_sub(input_height + 1);
    let transcript = render_entries(&state.entries, columns);
    let max_scroll = transcript.len().saturating_sub(transcript_height);
    state.scroll_offset = state.scroll_offset.min(max_scroll);
    let end = transcript.len().saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(transcript_height);
    let visible = &transcript[start..end];

    let mut frame = Vec::with_capacity(rows);
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        transcript_height.saturating_sub(visible.len()),
    ));
    frame.extend(
        visible
            .iter()
            .map(|line| markdown::fit_width(line, columns)),
    );
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
    let mut status = format!(
        " {}  {}/{} tokens ({}%)",
        state.model, state.context_tokens, state.context_window, percentage
    );
    if !state.activity.is_empty() {
        status.push_str("  ");
        status.push_str(&state.activity);
    }
    frame.push(format!(
        "\x1b[7m{}\x1b[0m",
        markdown::fit_width(&status, columns)
    ));

    let cursor_row = transcript_height + 2 + cursor_input_row;
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_keeps_input_and_status_pinned() {
        let mut state = ViewState {
            entries: vec![Entry::Assistant("hello".into())],
            streaming_assistant: None,
            running_tool: None,
            model: "test".into(),
            context_tokens: 12,
            context_window: 100,
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
        };
        let editor = Editor::default();
        let (frame, cursor) = build_frame(&mut state, &editor, 40, 12);
        assert_eq!(frame.len(), 12);
        assert!(markdown::strip_ansi(frame.last().unwrap()).contains("test"));
        assert_eq!(cursor.0, 10);
    }

    #[test]
    fn loaded_messages_become_transcript_entries() {
        let messages = vec![
            Message::user("hi"),
            Message::assistant("hello".into(), vec![]),
            Message::tool_result("id", "shell", "ok".into(), false),
        ];
        assert_eq!(entries_from_messages(&messages).len(), 3);
    }
}
