//! Focused tests for the corresponding TUI responsibility.

use super::picker::{
    SETTINGS_ACCENT_COLOR_INDEX, SETTINGS_AUTO_COMPACT_INDEX, SETTINGS_REASONING_DISPLAY_INDEX,
    SETTINGS_RELOAD_INDEX, SETTINGS_SCROLL_BAR_INDEX, settings_picker,
};
use super::*;

#[test]
fn picker_is_bounded_and_highlights_selection() {
    let picker = Picker {
        title: "Choose model".into(),
        hint: "Enter select".into(),
        selected: 1,
        items: vec![
            PickerItem {
                label: "First".into(),
                description: "provider:first".into(),
                action: PickerAction::SwitchModel("provider:first".into()),
            },
            PickerItem {
                label: "Second".into(),
                description: "provider:second".into(),
                action: PickerAction::SwitchModel("provider:second".into()),
            },
        ],
        editing: None,
    };
    let rendered = render_picker(&picker, &Editor::default(), 50, 10);
    assert_eq!(rendered.len(), 10);
    assert!(
        rendered
            .iter()
            .all(|line| markdown::visible_width(line) == 50)
    );
    assert!(
        rendered
            .iter()
            .any(|line| { line.contains("Second") && line.contains("\x1b[7m") })
    );
}

#[test]
fn accent_picker_selects_the_current_shared_color() {
    let blue = UiColor::new(117, 169, 255);
    let picker = color_picker(blue);

    assert_eq!(picker.title, "Accent color");
    assert!(matches!(
        picker.items[picker.selected].action,
        PickerAction::SetAccentColor(color) if color == blue
    ));
    assert!(picker.items.iter().any(|item| item.label == "Custom RGB…"));
}

#[test]
fn editable_setting_stays_in_the_picker_and_submits_without_a_slash_command() {
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
        completions: Vec::new(),
        completion_index: 0,
        picker: Some(Picker {
            title: "Settings".into(),
            hint: "Enter change".into(),
            selected: 0,
            items: vec![PickerItem {
                label: "Max output tokens".into(),
                description: "8192".into(),
                action: PickerAction::EditSetting {
                    key: "max_tokens".into(),
                    initial: "8192".into(),
                },
            }],
            editing: None,
        }),
    };
    let mut editor = Editor::default();

    assert!(take_picker_action(&mut state, &mut editor, Key::Enter).is_none());
    assert!(picker_is_editing(&state));
    assert_eq!(editor.text(), "8192");

    editor.clear();
    editor.paste("16384");
    let action = take_picker_action(&mut state, &mut editor, Key::Enter);
    assert!(matches!(
        action,
        Some(PickerAction::ApplySetting {
            argument,
            selected: 0
        }) if argument == "max_tokens 16384"
    ));
    assert!(state.picker.is_none());
}

#[test]
fn escape_cancels_picker_editing_and_dismisses_picker() {
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
        completions: Vec::new(),
        completion_index: 0,
        picker: Some(Picker {
            title: "Settings".into(),
            hint: "Enter change".into(),
            selected: 0,
            items: vec![PickerItem {
                label: "Max output tokens".into(),
                description: "8192".into(),
                action: PickerAction::EditSetting {
                    key: "max_tokens".into(),
                    initial: "8192".into(),
                },
            }],
            editing: None,
        }),
    };
    let mut editor = Editor::default();

    // Enter starts editing
    assert!(take_picker_action(&mut state, &mut editor, Key::Enter).is_none());
    assert!(picker_is_editing(&state));
    assert_eq!(editor.text(), "8192");

    // Esc cancels editing, clears text, but keeps the picker open
    assert!(take_picker_action(&mut state, &mut editor, Key::Escape).is_none());
    assert!(!picker_is_editing(&state));
    assert!(editor.is_empty());
    assert!(state.picker.is_some());

    // Second Esc dismisses the picker
    assert!(take_picker_action(&mut state, &mut editor, Key::Escape).is_none());
    assert!(state.picker.is_none());
}

#[test]
fn settings_picker_indexes_keep_their_action_contracts() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-picker-contract-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("test".into()),
        anthropic_base_url: String::new(),
        openai_base_url: String::new(),
        max_tokens: 8192,
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        scroll_bar: true,
        context_windows: std::collections::HashMap::new(),
        auto_compact: true,
        compact_threshold: 0.85,
        skill_dirs: Vec::new(),
        providers: std::collections::HashMap::new(),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
    };
    let session = crate::session::Session::create(&config.sessions_dir())
        .expect("test session should be created");
    let agent = Agent::new(config, "test".into(), session, Vec::new());

    let picker = settings_picker(&agent);

    assert_eq!(
        picker.items[SETTINGS_REASONING_DISPLAY_INDEX].label,
        "Reasoning display"
    );
    assert_eq!(
        picker.items[SETTINGS_ACCENT_COLOR_INDEX].label,
        "Accent color"
    );
    assert_eq!(picker.items[SETTINGS_SCROLL_BAR_INDEX].label, "Scroll bar");
    assert!(matches!(
        picker.items[SETTINGS_SCROLL_BAR_INDEX].action,
        PickerAction::SetScrollBar(false)
    ));
    assert_eq!(
        picker.items[SETTINGS_AUTO_COMPACT_INDEX].label,
        "Automatic compaction"
    );
    assert_eq!(
        picker.items[SETTINGS_RELOAD_INDEX].label,
        "Reload configuration"
    );

    drop(agent);
    let _ = std::fs::remove_dir_all(root);
}
