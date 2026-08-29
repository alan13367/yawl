//! Focused tests for the corresponding TUI responsibility.

use super::picker::{SettingsCategory, SettingsItem, settings_item_index};
use super::*;

#[test]
fn settings_and_model_pickers_are_recognized_during_an_active_turn() {
    assert_eq!(busy_command(" /settings "), Some(BusyCommand::Settings));
    assert_eq!(busy_command("/model"), Some(BusyCommand::Model));
    assert_eq!(busy_command("/connect"), Some(BusyCommand::Connect));
    assert_eq!(busy_command("/subagents"), Some(BusyCommand::Subagents));
    assert_eq!(busy_command("/ps"), Some(BusyCommand::Processes));
    assert_eq!(busy_command("/copy"), Some(BusyCommand::Copy));
    assert_eq!(busy_command("/copy-all"), Some(BusyCommand::CopyAll));
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
        parent: None,
    };
    let interface_count = 5;
    let settings = Picker {
        items: (0..interface_count)
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
        settings_categories: Vec::new(),
        reasoning: picker.clone(),
        default_reasoning: picker.clone(),
        accent_color: picker.clone(),
        selection_color: picker,
    };
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        scroll_bar_idle_ticks: 0,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        turn_started: None,
        context_tokens: 0,
        context_window: 100,
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
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
        render_cache: crate::tui::render::RenderCache::default(),
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
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    active_pickers.settings_categories = vec![(
        SettingsCategory::Interface,
        super::picker::settings_category_picker_from(
            &config,
            "test",
            100,
            SettingsCategory::Interface,
            0,
        ),
    )];

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
    let reasoning =
        settings_item_index(SettingsCategory::Interface, SettingsItem::ReasoningDisplay);
    assert_eq!(settings.selected, reasoning);
    assert_eq!(
        settings.items[reasoning].description,
        "Hidden · Enter to toggle"
    );
    assert!(matches!(
        settings.items[reasoning].action,
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
    let accent = settings_item_index(SettingsCategory::Interface, SettingsItem::AccentColor);
    assert_eq!(settings.selected, accent);
    assert_eq!(settings.items[accent].description, "blue");
    assert!(matches!(
        active_pickers.accent_color.items[active_pickers.accent_color.selected].action,
        PickerAction::SetAccentColor(color) if color == blue
    ));

    activate_picker_action_while_busy(
        &mut state,
        PickerAction::SetScrollBar(false),
        &mut active_pickers,
        &mut config,
    );

    assert!(!state.show_scroll_bar);
    assert!(!config.scroll_bar);
    let settings = state
        .picker
        .as_ref()
        .expect("settings picker should reopen");
    let scroll = settings_item_index(SettingsCategory::Interface, SettingsItem::ScrollBar);
    assert_eq!(settings.selected, scroll);
    assert_eq!(
        settings.items[scroll].description,
        "Hidden · Enter to toggle"
    );
    assert!(matches!(
        settings.items[scroll].action,
        PickerAction::SetScrollBar(true)
    ));

    activate_picker_action_while_busy(
        &mut state,
        PickerAction::SetScrollBarAutoHide(false),
        &mut active_pickers,
        &mut config,
    );

    assert!(!config.scroll_bar_auto_hide);
    let settings = state
        .picker
        .as_ref()
        .expect("settings picker should reopen");
    let auto_hide =
        settings_item_index(SettingsCategory::Interface, SettingsItem::ScrollBarAutoHide);
    assert_eq!(settings.selected, auto_hide);
    assert_eq!(
        settings.items[auto_hide].description,
        "Off · Enter to toggle"
    );
    assert!(matches!(
        settings.items[auto_hide].action,
        PickerAction::SetScrollBarAutoHide(true)
    ));

    activate_picker_action_while_busy(
        &mut state,
        PickerAction::OpenWebSearchProviders,
        &mut active_pickers,
        &mut config,
    );
    let provider_picker = state
        .picker
        .as_ref()
        .expect("search provider picker should open");
    assert_eq!(provider_picker.items.len(), 3);
    assert_eq!(provider_picker.selected, 0);
    assert!(state.pending_actions.is_empty());

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

    state.pending_actions.clear();
    state.queued_inputs.push_back("queued prompt".into());
    activate_picker_action_while_busy(
        &mut state,
        PickerAction::ApplyConnectionPlan(crate::onboarding::provider::ConnectionPlan {
            changes: vec![crate::config::ConfigChange::Provider {
                name: "new-provider".into(),
                base_url: "http://127.0.0.1:9999/v1".into(),
                api_key: Some("-".into()),
            }],
            model: "new-provider:test".into(),
            activation: crate::onboarding::provider::ConnectionActivation::Session,
            provider_label: "new-provider".into(),
        }),
        &mut active_pickers,
        &mut config,
    );

    assert_eq!(state.pending_actions.len(), 1);
    assert_eq!(
        state.queued_inputs.front().map(|input| input.text.as_str()),
        Some("queued prompt")
    );
    assert!(!config.providers.contains_key("new-provider"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn escape_and_ctrl_c_cancel_an_active_turn() {
    assert!(is_cancel_key(Key::Escape));
    assert!(is_cancel_key(Key::Ctrl('c')));
    assert!(!is_cancel_key(Key::Enter));
}

#[test]
fn submitted_long_paste_is_expanded_before_transcript_display() {
    let prompt = "long prompt ".repeat(400);
    let mut editor = Editor::default();
    editor.paste(&prompt);
    let EditAction::Submit(input) = editor.handle_key(Key::Enter) else {
        panic!("the non-empty editor should submit");
    };

    let displayed_input = displayed_submission(&editor, &input);
    let mut transcript = Transcript::from_messages(&[]);
    transcript.push_user(displayed_input);

    assert_eq!(transcript.entries(), &[Entry::User(prompt)]);
}
