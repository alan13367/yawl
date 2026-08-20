//! Focused tests for the corresponding TUI responsibility.

use super::picker::{SETTINGS_ACCENT_COLOR_INDEX, SETTINGS_REASONING_DISPLAY_INDEX};
use super::*;

#[test]
fn settings_and_model_pickers_are_recognized_during_an_active_turn() {
    assert_eq!(busy_command(" /settings "), Some(BusyCommand::Settings));
    assert_eq!(busy_command("/model"), Some(BusyCommand::Model));
    assert_eq!(
        busy_command("/unqueue 2"),
        Some(BusyCommand::Unqueue("2".into()))
    );
    assert_eq!(busy_command("/settings max_tokens 1"), None);
    assert_eq!(busy_command("hello"), None);
}

#[test]
fn display_settings_apply_during_an_active_turn() {
    let picker = Picker {
        title: "Settings".into(),
        hint: String::new(),
        selected: 0,
        items: Vec::new(),
        editing: None,
    };
    let settings = Picker {
        items: (0..=SETTINGS_ACCENT_COLOR_INDEX)
            .map(|index| PickerItem {
                label: format!("Setting {index}"),
                description: String::new(),
                action: PickerAction::ShowSettings,
            })
            .collect(),
        ..picker.clone()
    };
    let mut active_pickers = ActivePickers {
        model: picker.clone(),
        default_model: picker.clone(),
        settings,
        reasoning: picker.clone(),
        default_reasoning: picker.clone(),
        accent_color: picker,
    };
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
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
    };
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-live-settings-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let mut config = Config {
        model: Some("test".into()),
        anthropic_base_url: String::new(),
        openai_base_url: String::new(),
        max_tokens: 8192,
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        context_windows: std::collections::HashMap::new(),
        auto_compact: true,
        compact_threshold: 0.85,
        skill_dirs: Vec::new(),
        providers: std::collections::HashMap::new(),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
    };

    activate_picker_action_while_busy(
        &mut state,
        PickerAction::SetHideReasoning(true),
        &mut active_pickers,
        &mut config,
    );

    assert!(state.hide_reasoning);
    assert!(config.hide_reasoning);
    assert!(state.pending_actions.is_empty());
    assert!(config.global_config_path().exists());
    let settings = state
        .picker
        .as_ref()
        .expect("settings picker should reopen");
    assert_eq!(settings.selected, SETTINGS_REASONING_DISPLAY_INDEX);
    assert_eq!(
        settings.items[SETTINGS_REASONING_DISPLAY_INDEX].description,
        "Hidden · Enter to toggle"
    );
    assert!(matches!(
        settings.items[SETTINGS_REASONING_DISPLAY_INDEX].action,
        PickerAction::SetHideReasoning(false)
    ));

    let blue = UiColor::new(117, 169, 255);
    activate_picker_action_while_busy(
        &mut state,
        PickerAction::SetAccentColor(blue),
        &mut active_pickers,
        &mut config,
    );

    assert_eq!(state.accent_color, blue);
    assert_eq!(config.accent_color, blue);
    assert!(state.pending_actions.is_empty());
    let settings = state
        .picker
        .as_ref()
        .expect("settings picker should reopen");
    assert_eq!(settings.selected, SETTINGS_ACCENT_COLOR_INDEX);
    assert_eq!(
        settings.items[SETTINGS_ACCENT_COLOR_INDEX].description,
        "blue"
    );
    assert!(matches!(
        active_pickers.accent_color.items[active_pickers.accent_color.selected].action,
        PickerAction::SetAccentColor(color) if color == blue
    ));

    activate_picker_action_while_busy(
        &mut state,
        PickerAction::SetReasoning {
            effort: Some("high".into()),
            save: true,
        },
        &mut active_pickers,
        &mut config,
    );

    assert_eq!(state.pending_actions.len(), 1);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn escape_and_ctrl_c_cancel_an_active_turn() {
    assert!(is_cancel_key(Key::Escape));
    assert!(is_cancel_key(Key::Ctrl('c')));
    assert!(!is_cancel_key(Key::Enter));
}
