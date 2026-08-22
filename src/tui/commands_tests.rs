//! Focused tests for the corresponding TUI responsibility.

use super::*;

#[test]
fn new_and_clear_are_new_session_commands() {
    assert!(is_new_session_command("new"));
    assert!(is_new_session_command("clear"));
    assert!(!is_new_session_command("compact"));
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
        show_scroll_bar: true,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        context_tokens: 0,
        context_window: 100,
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: ["first".into(), "second".into()].into(),
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        picker: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_view: None,
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
