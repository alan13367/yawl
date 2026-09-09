//! Transcript-to-clipboard formatting and copy helpers.
//!
//! The commands facade owns slash-command dispatch; this child formats
//! conversation turns for the system clipboard. Image staging stays in
//! the separate `clipboard` module.

use crate::error::Error;
use crate::provider::{Message, Role};

use super::super::ViewState;
use super::super::{state, terminal, transcript};

pub(in crate::tui) fn last_assistant_reply<'a>(
    messages: &'a [Message],
    streaming: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(text) = streaming.filter(|text| !text.is_empty()) {
        return Some(text);
    }
    messages.iter().rev().find_map(|message| {
        (message.role == Role::Assistant && !message.content.is_empty())
            .then_some(message.content.as_str())
    })
}

pub(in crate::tui) fn format_copy_all(messages: &[Message], streaming: Option<&str>) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message.role {
            Role::User if message.is_hidden_control() => {}
            Role::User => parts.push(CopyPart::User(&message.content)),
            Role::Assistant => parts.push(CopyPart::Assistant(&message.content)),
            Role::Tool => {}
        }
    }
    if let Some(text) = streaming {
        parts.push(CopyPart::Assistant(text));
    }
    format_copy_parts(&parts)
}

pub(in crate::tui) fn format_copy_all_from_transcript(
    transcript: &transcript::Transcript,
) -> String {
    let mut parts = Vec::new();
    for entry in transcript.entries() {
        match entry {
            transcript::Entry::User(content) | transcript::Entry::Steer(content) => {
                parts.push(CopyPart::User(content))
            }
            transcript::Entry::Assistant(content) => {
                parts.push(CopyPart::Assistant(content));
            }
            transcript::Entry::SubagentResult {
                id,
                name,
                status,
                content,
            } => {
                parts.push(CopyPart::OwnedUser(format!(
                    "[{id} {status}] {name}\n{content}"
                )));
            }
            _ => {}
        }
    }
    format_copy_parts(&parts)
}

enum CopyPart<'a> {
    User(&'a str),
    Assistant(&'a str),
    OwnedUser(String),
}

fn format_copy_parts(parts: &[CopyPart<'_>]) -> String {
    let mut blocks = Vec::new();
    let mut pending_user: Option<String> = None;
    let mut assistants: Vec<String> = Vec::new();
    let mut saw_empty_assistant = false;
    for part in parts {
        match part {
            CopyPart::User(content) => {
                flush_copy_turn(
                    &mut blocks,
                    &mut pending_user,
                    &mut assistants,
                    &mut saw_empty_assistant,
                );
                pending_user = Some((*content).to_string());
            }
            CopyPart::OwnedUser(content) => {
                flush_copy_turn(
                    &mut blocks,
                    &mut pending_user,
                    &mut assistants,
                    &mut saw_empty_assistant,
                );
                pending_user = Some(content.clone());
            }
            CopyPart::Assistant("") => {
                saw_empty_assistant = true;
            }
            CopyPart::Assistant(content) => assistants.push((*content).to_string()),
        }
    }
    flush_copy_turn(
        &mut blocks,
        &mut pending_user,
        &mut assistants,
        &mut saw_empty_assistant,
    );
    blocks.join("\n\n")
}

fn flush_copy_turn(
    blocks: &mut Vec<String>,
    pending_user: &mut Option<String>,
    assistants: &mut Vec<String>,
    saw_empty_assistant: &mut bool,
) {
    if assistants.is_empty() && *saw_empty_assistant {
        pending_user.take();
        *saw_empty_assistant = false;
        return;
    }
    if let Some(user) = pending_user.take() {
        blocks.push(format!("User:\n{user}"));
    }
    for text in assistants.drain(..) {
        blocks.push(format!("Assistant:\n{text}"));
    }
    *saw_empty_assistant = false;
}

pub(in crate::tui) fn copy_to_clipboard(
    terminal: &mut terminal::Terminal,
    state: &mut ViewState,
    text: &str,
) -> Result<(), Error> {
    if text.is_empty() {
        state.notice("Nothing to copy.");
        return Ok(());
    }
    if terminal.copy_text(text)? {
        state.copy_toast_ticks = state::COPY_TOAST_TICKS;
    }
    Ok(())
}

pub(in crate::tui) fn copy_last_reply(
    terminal: &mut terminal::Terminal,
    state: &mut ViewState,
    messages: &[Message],
) -> Result<(), Error> {
    let text = state
        .transcript
        .last_assistant_text()
        .map(str::to_string)
        .or_else(|| last_assistant_reply(messages, None).map(str::to_string))
        .unwrap_or_default();
    copy_to_clipboard(terminal, state, &text)
}

pub(in crate::tui) fn copy_all_messages(
    terminal: &mut terminal::Terminal,
    state: &mut ViewState,
    messages: &[Message],
) -> Result<(), Error> {
    let streaming = state
        .transcript
        .has_streaming_assistant()
        .then(|| state.transcript.last_assistant_text().map(str::to_string))
        .flatten();
    let text = format_copy_all(messages, streaming.as_deref());
    copy_to_clipboard(terminal, state, &text)
}

pub(in crate::tui) fn copy_all_from_transcript(
    terminal: &mut terminal::Terminal,
    state: &mut ViewState,
) -> Result<(), Error> {
    let text = format_copy_all_from_transcript(&state.transcript);
    copy_to_clipboard(terminal, state, &text)
}
