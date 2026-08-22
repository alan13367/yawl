//! Focused tests for the corresponding TUI responsibility.

use super::*;

#[test]
fn enter_submits_the_only_matching_command_completion() {
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
        queued_inputs: std::collections::VecDeque::new(),
        pending_actions: std::collections::VecDeque::new(),
        completions: vec![Completion {
            command: "/quit".into(),
            description: "Exit Yawl".into(),
        }],
        completion_index: 0,
        picker: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_view: None,
    };
    let mut editor = Editor::default();
    editor.paste("/qui");

    assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
    assert_eq!(
        editor.handle_key(Key::Enter),
        EditAction::Submit("/quit ".into())
    );
}
