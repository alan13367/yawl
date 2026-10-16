//! Focused tests for the corresponding TUI responsibility.

use super::events::{MouseEvent, MouseKind};
use super::state::{
    SCROLL_BAR_AUTO_HIDE_TICKS, ScrollGeometry, Update, ViewState, advance_ticks,
    handle_scroll_bar_mouse, handle_tool_click, hover_scroll_bar, scroll, scroll_bar_span,
    toggle_tool_entry, toggle_tool_expansion,
};
use super::transcript::{Entry, Transcript, TranscriptEvent};
use crate::provider::{Message, ReasoningKind, ToolCall};

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

#[test]
fn scroll_bar_thumb_keeps_a_three_row_minimum_when_space_allows() {
    assert_eq!(scroll_bar_span(8, 1_000), (3, 5));
    assert_eq!(scroll_bar_span(2, 1_000), (2, 0));
}

fn state_with(geometry: Option<ScrollGeometry>) -> ViewState {
    ViewState {
        scroll_geometry: geometry,
        ..ViewState::test_default()
    }
}

#[test]
fn pointer_hovers_the_scroll_bar_anywhere_on_its_rail() {
    let mut state = state_with(Some(geometry()));
    hover_scroll_bar(&mut state, mouse(MouseKind::Move, 39, 0));
    assert!(state.scroll_bar_hovered, "the track scrolls on click too");
    hover_scroll_bar(&mut state, mouse(MouseKind::Move, 38, 0));
    assert!(!state.scroll_bar_hovered);
    hover_scroll_bar(&mut state, mouse(MouseKind::Move, 39, 8));
    assert!(!state.scroll_bar_hovered, "the composer is below the rail");

    let mut hidden = state_with(None);
    hover_scroll_bar(&mut hidden, mouse(MouseKind::Move, 39, 0));
    assert!(!hidden.scroll_bar_hovered);
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

fn auto_hidden_state(geometry: Option<ScrollGeometry>) -> ViewState {
    let mut state = state_with(geometry);
    state.scroll_bar_auto_hide = true;
    state
}

#[test]
fn scroll_bar_auto_hide_engages_after_the_idle_window() {
    let mut state = auto_hidden_state(None);
    assert!(state.show_scroll_bar);

    for _ in 1..SCROLL_BAR_AUTO_HIDE_TICKS {
        advance_ticks(&mut state);
        assert!(state.show_scroll_bar);
    }
    advance_ticks(&mut state);
    assert!(!state.show_scroll_bar);
}

#[test]
fn scrolling_resets_the_auto_hide_timer_and_reveals_the_bar() {
    let mut state = auto_hidden_state(None);
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS {
        advance_ticks(&mut state);
    }
    assert!(!state.show_scroll_bar);

    scroll(&mut state, 3);
    assert!(state.show_scroll_bar);
    assert_eq!(state.scroll_bar_idle_ticks, 0);

    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS - 1 {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);
    advance_ticks(&mut state);
    assert!(!state.show_scroll_bar);
}

#[test]
fn hovering_the_rail_keeps_an_auto_hiding_bar_visible() {
    let mut state = auto_hidden_state(Some(geometry()));
    hover_scroll_bar(&mut state, mouse(MouseKind::Move, 39, 2));
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS * 2 {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);

    // Leaving the rail starts a fresh idle window.
    hover_scroll_bar(&mut state, mouse(MouseKind::Move, 10, 2));
    for _ in 1..SCROLL_BAR_AUTO_HIDE_TICKS {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);
    advance_ticks(&mut state);
    assert!(!state.show_scroll_bar);
}

#[test]
fn an_active_scroll_bar_drag_pauses_auto_hide() {
    let mut state = auto_hidden_state(Some(geometry()));
    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Press, 39, 0)
    ));

    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS * 2 {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);

    assert!(handle_scroll_bar_mouse(
        &mut state,
        mouse(MouseKind::Release, 39, 0)
    ));
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS - 1 {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);
    advance_ticks(&mut state);
    assert!(!state.show_scroll_bar);
}

#[test]
fn auto_hide_stays_off_when_the_setting_is_disabled() {
    let mut state = state_with(None);
    state.scroll_bar_auto_hide = false;

    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS * 2 {
        advance_ticks(&mut state);
    }
    assert!(state.show_scroll_bar);
}

#[test]
fn auto_hide_never_reveals_a_disabled_scroll_bar() {
    let mut state = state_with(None);
    state.scroll_bar_enabled = false;
    state.show_scroll_bar = false;
    state.scroll_bar_auto_hide = true;

    scroll(&mut state, 3);
    assert!(!state.show_scroll_bar);
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS * 2 {
        advance_ticks(&mut state);
    }
    assert!(!state.show_scroll_bar);
}

#[test]
fn sync_scroll_bar_config_reapplies_settings_and_clears_the_timer() {
    let mut state = state_with(None);
    state.scroll_bar_enabled = false;
    state.scroll_bar_auto_hide = true;
    state.show_scroll_bar = false;
    state.scroll_bar_idle_ticks = SCROLL_BAR_AUTO_HIDE_TICKS;

    let mut config = crate::config::Config::test_default();
    config.scroll_bar = true;
    state.sync_scroll_bar_config(&config);

    assert!(state.scroll_bar_enabled);
    assert!(state.scroll_bar_auto_hide);
    assert!(state.show_scroll_bar);
    assert_eq!(state.scroll_bar_idle_ticks, 0);
}

#[test]
fn sync_scroll_bar_config_leaves_an_auto_hidden_bar_hidden() {
    let mut state = auto_hidden_state(None);
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS {
        advance_ticks(&mut state);
    }
    assert!(!state.show_scroll_bar);
    let idle = state.scroll_bar_idle_ticks;

    let mut config = crate::config::Config::test_default();
    config.scroll_bar = true;
    config.scroll_bar_auto_hide = true;
    state.sync_scroll_bar_config(&config);

    assert!(!state.show_scroll_bar);
    assert_eq!(state.scroll_bar_idle_ticks, idle);
}

#[test]
fn sync_scroll_bar_config_shows_the_bar_when_auto_hide_turns_off() {
    let mut state = auto_hidden_state(None);
    for _ in 0..SCROLL_BAR_AUTO_HIDE_TICKS {
        advance_ticks(&mut state);
    }
    assert!(!state.show_scroll_bar);

    let mut config = crate::config::Config::test_default();
    config.scroll_bar = true;
    config.scroll_bar_auto_hide = false;
    state.sync_scroll_bar_config(&config);

    assert!(state.show_scroll_bar);
    assert!(!state.scroll_bar_auto_hide);
    assert_eq!(state.scroll_bar_idle_ticks, 0);
}

fn owner(entry: usize, first: bool) -> super::render::LineOwner {
    super::render::LineOwner { entry, first }
}

fn two_tool_state() -> ViewState {
    let assistant = Message::assistant(
        String::new(),
        vec![
            ToolCall {
                id: "call-1".into(),
                name: "shell".into(),
                arguments: r#"{"command":"first"}"#.into(),
            },
            ToolCall {
                id: "call-2".into(),
                name: "shell".into(),
                arguments: r#"{"command":"second"}"#.into(),
            },
        ],
    );
    let mut state = state_with(None);
    state.transcript = Transcript::from_messages(&[
        assistant,
        Message::tool_result("call-1", "shell", "first output".into(), false),
        Message::tool_result("call-2", "shell", "second output".into(), false),
    ]);
    assert_eq!(state.transcript.entries().len(), 2);
    state
}

#[test]
fn clicking_a_tool_toggles_only_that_entry() {
    let mut state = two_tool_state();
    // Two collapsed tool cards: rows 0-1 belong to the first, row 3 to the second.
    state.transcript_row_entries = vec![
        Some(owner(0, true)),
        Some(owner(0, false)),
        None,
        Some(owner(1, true)),
        Some(owner(1, false)),
    ];
    assert!(!state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(!state.transcript.entry_expanded(1, state.tools_expanded));

    let press = mouse(MouseKind::Press, 2, 0);
    let release = mouse(MouseKind::Release, 2, 0);
    assert!(handle_tool_click(&mut state, press, release));
    assert!(state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(!state.transcript.entry_expanded(1, state.tools_expanded));

    // Clicking the same card again collapses only it.
    assert!(handle_tool_click(&mut state, press, release));
    assert!(!state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(!state.transcript.entry_expanded(1, state.tools_expanded));
}

#[test]
fn clicking_the_second_tool_leaves_the_first_collapsed() {
    let mut state = two_tool_state();
    state.transcript_row_entries = vec![Some(owner(0, true)), Some(owner(1, true))];

    assert!(handle_tool_click(
        &mut state,
        mouse(MouseKind::Press, 0, 1),
        mouse(MouseKind::Release, 0, 1),
    ));
    assert!(!state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(state.transcript.entry_expanded(1, state.tools_expanded));
}

#[test]
fn drags_and_non_tool_rows_never_toggle() {
    let mut state = two_tool_state();
    state.transcript_row_entries = vec![Some(owner(0, true)), Some(owner(0, false))];

    // A drag across cells stays reserved for text selection.
    assert!(!handle_tool_click(
        &mut state,
        mouse(MouseKind::Press, 1, 0),
        mouse(MouseKind::Release, 5, 0),
    ));
    assert!(!state.transcript.entry_expanded(0, state.tools_expanded));

    // Separator rows and clicks outside the transcript do nothing.
    state.transcript_row_entries = vec![Some(owner(0, true)), None];
    assert!(!handle_tool_click(
        &mut state,
        mouse(MouseKind::Press, 0, 1),
        mouse(MouseKind::Release, 0, 1),
    ));
    assert!(!handle_tool_click(
        &mut state,
        mouse(MouseKind::Press, 0, 7),
        mouse(MouseKind::Release, 0, 7),
    ));

    // Non-tool entries (prompts, replies) never toggle.
    let mut mixed = state_with(None);
    mixed.transcript = Transcript::from_messages(&[Message::user("hello"), Message::user("world")]);
    mixed.transcript_row_entries = vec![Some(owner(0, true)), Some(owner(1, true))];
    assert!(!toggle_tool_entry(&mut mixed, 0));
    assert!(!toggle_tool_entry(&mut mixed, 7));
}

#[test]
fn ctrl_o_expands_all_after_a_single_click_then_collapses_all() {
    let mut state = two_tool_state();
    state.transcript_row_entries = vec![Some(owner(0, true)), Some(owner(1, true))];

    // Click expands only the first card while the second stays collapsed.
    assert!(handle_tool_click(
        &mut state,
        mouse(MouseKind::Press, 0, 0),
        mouse(MouseKind::Release, 0, 0),
    ));
    assert!(state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(!state.transcript.entry_expanded(1, state.tools_expanded));

    // Ctrl+O expands everything, keeping the already-expanded card expanded.
    toggle_tool_expansion(&mut state);
    assert!(state.tools_expanded);
    assert!(state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(state.transcript.entry_expanded(1, state.tools_expanded));

    // Ctrl+O again collapses everything, including the clicked card.
    toggle_tool_expansion(&mut state);
    assert!(!state.tools_expanded);
    assert!(!state.transcript.entry_expanded(0, state.tools_expanded));
    assert!(!state.transcript.entry_expanded(1, state.tools_expanded));
}

fn reasoning_state() -> ViewState {
    let mut state = state_with(None);
    state.apply(Update::Transcript(TranscriptEvent::ReasoningDelta {
        kind: ReasoningKind::Full,
        text: "thought".into(),
    }));
    state
}

#[test]
fn clicking_a_thinking_tag_expands_it_while_tool_output_is_expanded() {
    let mut state = reasoning_state();
    state.apply(Update::Transcript(TranscriptEvent::AssistantDone));
    // Ctrl+O expands tool output; thinking tags default to collapsed.
    toggle_tool_expansion(&mut state);
    assert!(state.tools_expanded);
    assert!(!state.transcript.entry_expanded(0, false));

    // The click must use the reasoning default, not `tools_expanded`.
    assert!(toggle_tool_entry(&mut state, 0));
    assert!(state.transcript.entry_expanded(0, false));
}

#[test]
fn tool_preparing_freezes_the_streaming_thinking_duration() {
    let mut state = reasoning_state();
    state.apply(Update::ToolPreparing {
        name: "shell".into(),
    });
    match state.transcript.entry(0) {
        Some(Entry::Reasoning {
            started, duration, ..
        }) => {
            assert!(started.is_none(), "the tag stops ticking");
            assert!(duration.is_some(), "the duration matches the record");
        }
        other => panic!("expected a reasoning entry, got {other:?}"),
    }
}

#[test]
fn cancel_pauses_waiting_messages_until_explicit_delivery() {
    let mut state = state_with(None);
    state.queued_inputs.push_back("queued instruction".into());
    state.pending_steers.push_back("waiting steer".into());
    let token = crate::cancellation::CancellationToken::default();
    super::worker::cancel_worker(super::worker::native_thread_id(), &token, &mut state);
    assert!(token.is_canceled());
    assert!(state.queue_paused);
    assert_eq!(state.queued_inputs[0].text, "queued instruction");
    assert_eq!(state.pending_steers[0].text, "waiting steer");
    assert!(super::commands::promote_queued(&mut state, 0));
    assert!(!state.queue_paused);
}
