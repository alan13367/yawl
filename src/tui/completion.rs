//! Slash-command and `@` file-mention completion behavior.

use crate::agent::Agent;

use super::ViewState;
use super::events::Key;
use super::input::Editor;

/// Visible rows in the completion menu. The full match list is still
/// reachable by wrapping with Up/Down.
pub(super) const COMPLETION_MENU_ROWS: usize = 6;

#[derive(Clone)]
pub(super) struct Completion {
    pub(super) command: String,
    pub(super) description: String,
}

pub(super) fn command_completions(agent: &Agent) -> Vec<Completion> {
    let mut completions = [
        ("/model", "List or switch models"),
        ("/connect", "Configure a model provider"),
        ("/settings", "Show or change settings"),
        ("/subagents", "Open the subagent dashboard"),
        ("/ps", "Open the background process dashboard"),
        ("/new", "Start a session without changing directories"),
        ("/clear", "Alias for /new"),
        ("/compact", "Summarize older messages"),
        ("/undo", "Undo the last turn"),
        ("/copy", "Copy the last assistant reply"),
        ("/copy-all", "Copy the conversation without reasoning"),
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

pub(super) fn matching_completions<'a>(
    completions: &'a [Completion],
    editor: &Editor,
) -> Vec<&'a Completion> {
    let Some(prefix) = editor.command_prefix() else {
        return Vec::new();
    };
    completions
        .iter()
        .filter(|completion| completion.command.starts_with(&prefix))
        .collect()
}

/// Visible slice of matches that keeps `selected` on screen.
pub(super) fn completion_window(
    match_count: usize,
    selected: usize,
    capacity: usize,
) -> std::ops::Range<usize> {
    if match_count == 0 || capacity == 0 {
        return 0..0;
    }
    let capacity = capacity.min(match_count);
    let selected = selected.min(match_count - 1);
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    start..start + capacity
}

/// The token driving the completion menu: a slash command, or a mention
/// query rendered with its `@` sigil so the two never collide.
fn active_filter(editor: &Editor) -> Option<String> {
    editor
        .command_prefix()
        .or_else(|| editor.mention_prefix().map(|query| format!("@{query}")))
}

pub(super) fn sync_completion_filter(state: &mut ViewState, editor: &Editor) {
    let prefix = active_filter(editor);
    if state.completion_filter.as_deref() != prefix.as_deref() {
        state.completion_index = 0;
        state.completion_filter = prefix;
    }
}

/// Rows for the completion menu as `(label, detail)` pairs. Mention rows
/// lazily build the file index the first time `@` is typed.
pub(super) fn menu_rows(state: &mut ViewState, editor: &Editor) -> Vec<(String, String)> {
    if editor.command_prefix().is_some() {
        return matching_completions(&state.completions, editor)
            .into_iter()
            .map(|completion| (completion.command.clone(), completion.description.clone()))
            .collect();
    }
    if let Some(query) = editor.mention_prefix() {
        return state
            .file_index
            .matches(&query)
            .into_iter()
            .map(|path| mention_row(&path))
            .collect();
    }
    Vec::new()
}

/// Menu presentation for one file: the name up front, its directory as the
/// detail column.
fn mention_row(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some((directory, name)) => (name.to_string(), directory.to_string()),
        None => (path.to_string(), String::new()),
    }
}

pub(super) fn handle_completion_key(state: &mut ViewState, editor: &mut Editor, key: Key) -> bool {
    sync_completion_filter(state, editor);
    if editor.command_prefix().is_some() {
        return handle_command_key(state, editor, key);
    }
    if let Some(query) = editor.mention_prefix() {
        return handle_mention_key(state, editor, &query, key);
    }
    false
}

fn handle_command_key(state: &mut ViewState, editor: &mut Editor, key: Key) -> bool {
    let matches = matching_completions(&state.completions, editor)
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
            state.completion_index = if state.completion_index == 0 {
                matches.len() - 1
            } else {
                state.completion_index - 1
            };
            true
        }
        Key::Down => {
            state.completion_index = if state.completion_index + 1 == matches.len() {
                0
            } else {
                state.completion_index + 1
            };
            true
        }
        Key::Tab => {
            editor.complete_command(&matches[state.completion_index]);
            sync_completion_filter(state, editor);
            true
        }
        Key::Enter => {
            let typed = editor.command_prefix().unwrap_or_default();
            let command = matches
                .iter()
                .find(|command| command.as_str() == typed)
                .unwrap_or(&matches[state.completion_index]);
            editor.complete_command(command);
            state.completion_index = 0;
            false
        }
        _ => false,
    }
}

/// Keys for the `@` mention menu. Unlike slash commands, Enter inserts the
/// tag and keeps editing instead of submitting.
fn handle_mention_key(state: &mut ViewState, editor: &mut Editor, query: &str, key: Key) -> bool {
    let matches = state.file_index.matches(query);
    if matches.is_empty() {
        state.completion_index = 0;
        return false;
    }
    state.completion_index = state.completion_index.min(matches.len() - 1);
    match key {
        Key::Up => {
            state.completion_index = if state.completion_index == 0 {
                matches.len() - 1
            } else {
                state.completion_index - 1
            };
            true
        }
        Key::Down => {
            state.completion_index = if state.completion_index + 1 == matches.len() {
                0
            } else {
                state.completion_index + 1
            };
            true
        }
        Key::Tab | Key::Enter => {
            editor.complete_mention(&matches[state.completion_index]);
            state.completion_index = 0;
            sync_completion_filter(state, editor);
            true
        }
        _ => false,
    }
}
