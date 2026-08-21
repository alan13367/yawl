//! Slash-command completion catalog and input behavior.

use crate::agent::Agent;

use super::ViewState;
use super::events::Key;
use super::input::Editor;

#[derive(Clone)]
pub(super) struct Completion {
    pub(super) command: String,
    pub(super) description: String,
}

pub(super) fn command_completions(agent: &Agent) -> Vec<Completion> {
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
        .take(8)
        .collect()
}

pub(super) fn handle_completion_key(state: &mut ViewState, editor: &mut Editor, key: Key) -> bool {
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
