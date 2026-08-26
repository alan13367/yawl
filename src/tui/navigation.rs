//! Keyboard navigation for transcript blocks, search, and the focused viewer.

use crate::error::Error;

use super::events::Key;
use super::state::{COPY_TOAST_TICKS, ViewState, scroll};
use super::terminal::Terminal;

/// Handles keys owned by transcript navigation. A `true` result means the
/// composer must not see the key.
pub(super) fn handle_key(
    state: &mut ViewState,
    terminal: &mut Terminal,
    key: Key,
) -> Result<bool, Error> {
    if state.transcript.search_active() {
        match key {
            Key::Escape | Key::Ctrl('c') | Key::Ctrl('f') => state.transcript.close_search(),
            Key::Enter | Key::Down => {
                state.transcript.search_next(false);
                set_expanded(state, true);
            }
            Key::Up => {
                state.transcript.search_next(true);
                set_expanded(state, true);
            }
            Key::Backspace => state.transcript.search_backspace(),
            Key::Char(character) => state.transcript.search_push(character),
            _ => {}
        }
        return Ok(true);
    }

    if state.transcript.viewer_open() {
        match key {
            Key::Escape | Key::Enter | Key::Ctrl('c') => {
                state.transcript.close_viewer();
                state.scroll_offset = 0;
            }
            Key::Up | Key::Char('k') => scroll(state, 1),
            Key::Down | Key::Char('j') => scroll(state, -1),
            Key::PageUp => scroll(state, 10),
            Key::PageDown => scroll(state, -10),
            Key::Home => state.scroll_offset = usize::MAX,
            Key::End => state.scroll_offset = 0,
            Key::Char('y') => copy_selected(state, terminal)?,
            _ => {}
        }
        return Ok(true);
    }

    if key == Key::Ctrl('f') {
        state.transcript.open_search();
        return Ok(true);
    }

    if !state.transcript.is_focused() {
        return Ok(false);
    }

    match key {
        Key::Tab | Key::Escape | Key::Ctrl('c') => state.transcript.blur(),
        Key::Up | Key::Char('k') => state.transcript.move_selection(-1),
        Key::Down | Key::Char('j') => state.transcript.move_selection(1),
        Key::Left | Key::Char('h') => set_expanded(state, false),
        Key::Right | Key::Char('l') => set_expanded(state, true),
        Key::Enter => {
            state.transcript.open_viewer();
            state.scroll_offset = 0;
        }
        Key::Char('y') => copy_selected(state, terminal)?,
        Key::PageUp => scroll(state, 10),
        Key::PageDown => scroll(state, -10),
        _ => {}
    }
    Ok(true)
}

pub(super) fn focus_transcript(state: &mut ViewState) {
    if state.transcript.is_empty() {
        return;
    }
    state.transcript.focus();
}

fn set_expanded(state: &mut ViewState, expanded: bool) {
    if state.transcript.set_selected_expanded(expanded) {
        state.render_cache.invalidate();
    }
}

fn copy_selected(state: &mut ViewState, terminal: &mut Terminal) -> Result<(), Error> {
    let Some(text) = state
        .transcript
        .selected_entry()
        .map(super::transcript::Entry::copy_text)
    else {
        return Ok(());
    };
    if terminal.copy_text(&text)? {
        state.copy_toast_ticks = COPY_TOAST_TICKS;
    }
    Ok(())
}
