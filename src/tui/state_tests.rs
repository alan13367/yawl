//! Focused tests for the corresponding TUI responsibility.

use super::events::{MouseEvent, MouseKind};
use super::state::{
    SCROLL_BAR_AUTO_HIDE_TICKS, ScrollGeometry, ViewState, advance_ticks, handle_scroll_bar_mouse,
    scroll, scroll_bar_span,
};
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

#[test]
fn scroll_bar_thumb_keeps_a_three_row_minimum_when_space_allows() {
    assert_eq!(scroll_bar_span(8, 1_000), (3, 5));
    assert_eq!(scroll_bar_span(2, 1_000), (2, 0));
}

fn state_with(geometry: Option<ScrollGeometry>) -> ViewState {
    ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        status_bar: Default::default(),
        status_bar_draft: None,
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        scroll_bar_idle_ticks: 0,
        scroll_geometry: geometry,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        turn_started: None,
        context_tokens: 0,
        context_window: 100,
        usage: crate::provider::UsageSummary::default(),
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_steers: std::collections::VecDeque::new(),
        active_goal: None,
        goal_running: false,
        active_plan: None,
        plan_draft: false,
        pending_plan_implementation: false,
        question: None,
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        completion_filter: None,
        file_index: crate::tui::files::FileIndex::default(),
        picker: None,
        connection: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
        background_processes: crate::background::BackgroundProcessManager::default(),
        background_active_count: 0,
        process_view: None,
        render_cache: super::render::RenderCache::default(),
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
