//! Focused tests for the corresponding TUI responsibility.

use super::events::{MouseEvent, MouseKind};
use super::state::{ScrollGeometry, ViewState, handle_scroll_bar_mouse};
use super::transcript::Transcript;
use crate::config::UiColor;

fn mouse(kind: MouseKind, column: usize, row: usize) -> MouseEvent {
    MouseEvent { kind, column, row }
}

fn geometry() -> ScrollGeometry {
    ScrollGeometry {
        rows: 8,
        columns: 40,
        max_scroll: 52,
        travel: 7,
        thumb_length: 1,
    }
}

fn state_with(geometry: Option<ScrollGeometry>) -> ViewState {
    ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        show_scroll_bar: true,
        scroll_geometry: geometry,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        context_tokens: 0,
        context_window: 100,
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        picker: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_view: None,
    }
}

#[test]
fn pressing_the_thumb_starts_a_drag_that_scrubs_the_transcript() {
    let mut state = state_with(Some(geometry()));
    // At the bottom the one-row thumb sits on the last transcript row.
    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 39, 7)
    ));
    assert_eq!(state.scroll_offset, 0);
    assert_eq!(state.scroll_bar_drag, Some(0));

    // Dragging upward moves toward older content.
    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Drag, 30, 0)
    ));
    assert_eq!(state.scroll_offset, 52);

    // Release ends the drag; later motion belongs to text selection again.
    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Release, 30, 0)
    ));
    assert_eq!(state.scroll_bar_drag, None);
    assert!(!handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Drag, 31, 1)
    ));
}

#[test]
fn pressing_the_track_centers_the_thumb_on_the_click() {
    let mut state = state_with(Some(geometry()));

    // Clicking far from the thumb jumps so the grab point stays put.
    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 39, 0)
    ));
    assert_eq!(state.scroll_offset, 52);
}

#[test]
fn presses_off_the_bar_fall_through_to_text_selection() {
    let mut state = state_with(Some(geometry()));

    assert!(!handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 10, 7)
    ));
    assert!(!handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 38, 8)
    ));
    assert_eq!(state.scroll_offset, 0);
}

#[test]
fn presses_are_ignored_when_no_bar_is_rendered() {
    let mut state = state_with(None);

    assert!(!handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 39, 7)
    ));
    assert_eq!(state.scroll_offset, 0);
}
