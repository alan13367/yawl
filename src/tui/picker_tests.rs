//! Focused tests for the corresponding TUI responsibility.

use super::picker::{
    SettingsCategory, SettingsItem, SettingsLocation, model_picker, refreshed_model_picker,
    settings_category_picker, settings_picker, status_bar_add_picker, status_bar_editor_picker,
    web_search_provider_picker,
};
use super::*;

#[test]
fn model_refresh_keeps_manual_model_entry_selected() {
    let config = test_agent().config().clone();
    for save in [false, true] {
        let mut picker = model_picker(&config, "test", save);
        picker.selected = picker.items.len() - 1;

        let refreshed = refreshed_model_picker(&config, "test", save, &picker);

        assert_eq!(refreshed.selected, refreshed.items.len() - 1);
        assert!(matches!(
            refreshed.items[refreshed.selected].action,
            PickerAction::EditModel { save: selected, .. } if selected == save
        ));
    }
}

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
        parent: None,
    };
    let rendered = render_picker(
        &picker,
        &Editor::default(),
        "\x1b[7m",
        "\x1b[38;2;116;199;213m",
        50,
        10,
    );
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
fn picker_uses_available_width_and_accent_colored_outline() {
    let model = "omlx:mtplx-qwen38-27b-optimized-speed-with-a-long-model-name";
    let picker = Picker {
        title: "Choose model".into(),
        hint: "Enter select".into(),
        selected: 0,
        items: vec![PickerItem {
            label: "MTPLX Qwen3.8-27B Optimized Speed".into(),
            description: model.into(),
            action: PickerAction::SwitchModel(model.into()),
        }],
        editing: None,
        parent: None,
    };
    let outline = "\x1b[38;2;117;169;255m";

    let rendered = render_picker(&picker, &Editor::default(), "\x1b[7m", outline, 129, 8);
    let panel = rendered.join("\n");

    assert!(panel.contains(model));
    assert!(panel.contains(&format!("{outline}┌")));
    assert!(panel.contains(&format!("{outline}│")));
    assert!(panel.contains(&format!("{outline}└")));
    assert!(
        rendered
            .iter()
            .all(|line| markdown::visible_width(line) == 129)
    );
    let top_border = rendered
        .iter()
        .find(|line| line.contains('┌'))
        .expect("picker should render a top border");
    assert_eq!(
        markdown::strip_ansi(top_border).trim_end().chars().count(),
        127
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
fn selection_picker_defaults_to_following_the_accent() {
    let picker = selection_color_picker(None);
    assert_eq!(picker.title, "Selection color");
    assert_eq!(picker.items[picker.selected].label, "Accent");
    assert!(matches!(
        picker.items[picker.selected].action,
        PickerAction::SetSelectionColor(None)
    ));

    let green = UiColor::new(139, 213, 162);
    let picker = selection_color_picker(Some(green));
    assert!(matches!(
        picker.items[picker.selected].action,
        PickerAction::SetSelectionColor(Some(color)) if color == green
    ));
    assert!(picker.items.iter().any(|item| item.label == "Custom RGB…"));
}

#[test]
fn accent_picker_previews_the_highlighted_color_and_restores_on_cancel() {
    let blue = UiColor::new(117, 169, 255);
    let mut state = test_picker_state(color_picker(UiColor::WHITE));
    state.accent_color = UiColor::WHITE;
    state.selection_color = UiColor::WHITE;
    state.begin_color_preview(None);

    // Down moves the highlight from White to Gray, previewing it live.
    assert!(take_picker_action(&mut state, &mut Editor::default(), Key::Down).is_none());
    sync_color_preview(&mut state);
    assert_eq!(state.accent_color, UiColor::new(148, 148, 158));
    assert_eq!(
        state.selection_color,
        UiColor::new(148, 148, 158),
        "a selection that follows the accent tracks the preview"
    );

    // Escape restores the committed colors instead of keeping the preview,
    // then returns to the Interface settings page.
    let cancel = take_picker_action(&mut state, &mut Editor::default(), Key::Escape);
    assert!(matches!(
        cancel,
        Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Interface,
            ..
        })
    ));
    assert_eq!(state.accent_color, UiColor::WHITE);
    assert_eq!(state.selection_color, UiColor::WHITE);
    assert!(state.color_preview.is_none());

    // Selecting a row commits the color under the highlight.
    let mut state = test_picker_state(color_picker(UiColor::WHITE));
    state.accent_color = blue;
    state.selection_color = blue;
    state.begin_color_preview(Some(blue));
    let selected = state
        .picker
        .as_ref()
        .expect("picker should be open")
        .items
        .iter()
        .position(
            |item| matches!(item.action, PickerAction::SetAccentColor(color) if color == blue),
        )
        .expect("blue should be in the palette");
    state.picker.as_mut().expect("picker").selected = selected;
    sync_color_preview(&mut state);
    assert_eq!(state.accent_color, blue);
    assert_eq!(state.selection_color, blue);
}

#[test]
fn selection_picker_previews_an_explicit_color_and_falls_back_while_editing() {
    let green = UiColor::new(139, 213, 162);
    let mut state = test_picker_state(selection_color_picker(None));
    state.accent_color = UiColor::new(117, 169, 255);
    state.selection_color = state.accent_color;
    state.begin_color_preview(None);

    let selected = state
        .picker
        .as_ref()
        .expect("picker should be open")
        .items
        .iter()
        .position(
            |item| matches!(item.action, PickerAction::SetSelectionColor(Some(color)) if color == green),
        )
        .expect("green should be in the palette");
    state.picker.as_mut().expect("picker").selected = selected;
    sync_color_preview(&mut state);
    assert_eq!(state.selection_color, green);
    assert_eq!(
        state.accent_color,
        UiColor::new(117, 169, 255),
        "a selection preview leaves the accent alone"
    );

    // The custom-entry row is an edit, not a color, so the preview reverts to
    // the committed colors rather than showing a stale swatch.
    let custom = state.picker.as_ref().expect("picker").items.len() - 1;
    state.picker.as_mut().expect("picker").selected = custom;
    sync_color_preview(&mut state);
    assert_eq!(state.selection_color, UiColor::new(117, 169, 255));
}

#[test]
fn closing_a_color_picker_keeps_the_applied_color() {
    let blue = UiColor::new(117, 169, 255);
    let mut state = test_picker_state(color_picker(UiColor::WHITE));
    state.accent_color = blue;
    state.selection_color = blue;
    state.begin_color_preview(None);
    state.picker = Some(settings_picker(&test_agent()));

    sync_color_preview(&mut state);
    assert!(state.color_preview.is_none());
    assert_eq!(
        state.accent_color, blue,
        "leaving a color picker must not revert the applied color"
    );
}

#[test]
fn a_rejected_custom_color_ends_the_preview() {
    let mut agent = test_agent();
    let mut state = test_picker_state(color_picker(UiColor::WHITE));
    state.accent_color = UiColor::WHITE;
    state.selection_color = UiColor::WHITE;
    state.begin_color_preview(agent.config().selection_color);

    // Highlight "Custom RGB…", edit it, and submit a value that cannot parse.
    let custom = state.picker.as_ref().expect("picker").items.len() - 1;
    state.picker.as_mut().expect("picker").selected = custom;
    let mut editor = Editor::default();
    take_picker_action(&mut state, &mut editor, Key::Enter);
    editor.clear();
    editor.paste("not-a-color");
    let action = take_picker_action(&mut state, &mut editor, Key::Enter);
    let argument = match action.expect("a custom value submits a setting change") {
        PickerAction::ApplySetting { argument, .. } => argument,
        _ => panic!("expected a setting change"),
    };
    assert_eq!(argument, "accent_color not-a-color");

    assert!(!super::commands::settings(
        &mut agent, &argument, &mut state
    ));
    assert!(state.picker.is_none());
    assert!(
        state.color_preview.is_none(),
        "a rejected value closes the picker, so nothing may keep previewing"
    );
    assert_eq!(state.accent_color, UiColor::WHITE);
    assert_eq!(state.transcript_accent_color(), state.accent_color);
}

#[test]
fn interface_settings_open_the_status_bar_editor() {
    let config = Config::test_default();
    let picker = super::picker::settings_category_picker_from(
        &config,
        "test",
        100,
        SettingsCategory::Interface,
        0,
    );

    let item = picker
        .items
        .iter()
        .find(|item| item.label == "Status bar")
        .expect("interface settings should list the status bar");
    assert!(matches!(
        item.action,
        PickerAction::OpenStatusBarEditor { .. }
    ));
}

#[test]
fn status_bar_editor_lists_unique_items_and_returns_move_and_remove_actions() {
    let mut state = test_picker_state(Picker {
        title: String::new(),
        hint: String::new(),
        items: Vec::new(),
        selected: 0,
        editing: None,
        parent: None,
    });
    state.status_bar_draft = Some(crate::config::StatusBarConfig {
        items: vec![crate::config::StatusBarItemConfig::new(
            crate::config::StatusBarKind::Model,
        )],
        ..Default::default()
    });
    state.picker = Some(status_bar_editor_picker(&state, 0));

    let move_action = take_picker_action(&mut state, &mut Editor::default(), Key::Char('J'));
    assert!(matches!(
        move_action,
        Some(PickerAction::MoveStatusBarItem {
            index: 0,
            direction: 1
        })
    ));

    state.picker = Some(status_bar_editor_picker(&state, 0));
    let remove_action = take_picker_action(&mut state, &mut Editor::default(), Key::Delete);
    assert!(matches!(
        remove_action,
        Some(PickerAction::RemoveStatusBarItem(0))
    ));

    let add = status_bar_add_picker(&state);
    assert!(!add.items.iter().any(|item| item.label == "Model"));
    assert!(add.items.iter().any(|item| item.label == "Prompt cache"));
}

#[test]
fn status_bar_label_editor_can_submit_an_empty_label() {
    let mut state = test_picker_state(Picker {
        title: "Model".into(),
        hint: String::new(),
        items: vec![PickerItem {
            label: "Custom label…".into(),
            description: String::new(),
            action: PickerAction::EditStatusBarLabel {
                index: 0,
                initial: String::new(),
            },
        }],
        selected: 0,
        editing: None,
        parent: None,
    });
    let mut editor = Editor::default();

    assert!(take_picker_action(&mut state, &mut editor, Key::Enter).is_none());
    let action = take_picker_action(&mut state, &mut editor, Key::Enter);

    assert!(matches!(
        action,
        Some(PickerAction::ApplyStatusBarLabel { index: 0, label }) if label.is_empty()
    ));
}

#[test]
fn escape_returns_the_typed_parent_action() {
    let parent = PickerAction::OpenSettingsRoot { selected: 3 };
    let mut state = test_picker_state(Picker {
        title: "Settings · Providers".into(),
        hint: "Esc back".into(),
        items: Vec::new(),
        selected: 0,
        editing: None,
        parent: Some(parent),
    });

    let action = take_picker_action(&mut state, &mut Editor::default(), Key::Escape);

    assert!(matches!(
        action,
        Some(PickerAction::OpenSettingsRoot { selected: 3 })
    ));
}

#[test]
fn plan_handoff_number_keys_choose_before_confirmation() {
    let mut state = test_picker_state(super::commands::plan_handoff_picker());
    let mut editor = Editor::default();

    assert!(take_picker_action(&mut state, &mut editor, Key::Char('2')).is_none());
    assert_eq!(state.picker.as_ref().map(|picker| picker.selected), Some(1));

    let action = take_picker_action(&mut state, &mut editor, Key::Enter);
    assert!(matches!(action, Some(PickerAction::ReturnFromPlan)));
}

#[test]
fn secret_picker_edit_masks_the_row_and_editor_layout() {
    let secret = "sk-secret-value";
    let mut editor = Editor::default();
    editor.paste(secret);
    let picker = Picker {
        title: "API key".into(),
        hint: String::new(),
        items: vec![PickerItem {
            label: "API key".into(),
            description: "masked".into(),
            action: PickerAction::ShowSettings,
        }],
        selected: 0,
        editing: Some(super::picker::PickerEdit::Connect {
            field: super::connection::ConnectEditField::Secret,
            secret: true,
        }),
        parent: None,
    };

    let panel = render_picker(
        &picker,
        &editor,
        "\x1b[7m",
        "\x1b[38;2;116;199;213m",
        60,
        10,
    )
    .join("\n");
    let input = editor.masked_layout(60).lines.join("\n");

    assert!(!panel.contains(secret));
    assert!(!input.contains(secret));
    assert!(panel.contains('•'));
    assert!(input.contains('•'));
}

fn test_picker_state(picker: Picker) -> ViewState {
    let mut state = ViewState::from_agent(&test_agent());
    state.picker = Some(picker);
    state
}

fn test_agent() -> Agent {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("yawl-picker-agent-{}-{nonce}", std::process::id()));
    let config = Config {
        model: Some("test".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let session = crate::session::Session::create(&config.session_dirs(&cwd).project, &cwd, "test")
        .expect("test session should be created");
    Agent::new(config, "test".into(), session, Vec::new())
}

#[test]
fn compatible_connection_toggles_with_space_and_confirms_with_enter() {
    use super::connection::{self, ConnectEditField, ConnectStep};
    use crate::onboarding::provider::ProviderId;

    let agent = test_agent();
    let mut state = ViewState::from_agent(&agent);
    connection::open(&mut state, agent.config(), false);
    connection::handle_action(
        &mut state,
        PickerAction::ConnectChooseProvider(ProviderId::Compatible("local".into())),
    );
    connection::handle_action(
        &mut state,
        PickerAction::ConnectChooseModel("custom".into()),
    );
    let picker = state.picker.as_ref().unwrap();
    assert_eq!(
        picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        [
            "[x] minimal",
            "[x] low",
            "[x] medium",
            "[x] high",
            "[x] xhigh",
            "[x] max",
            "[x] ultra"
        ]
    );
    let mut editor = Editor::default();
    take_picker_action(&mut state, &mut editor, Key::Down);
    for expected in ["[ ] low", "[x] low"] {
        let action = take_picker_action(&mut state, &mut editor, Key::Char(' ')).unwrap();
        assert!(matches!(action, PickerAction::ConnectToggleReasoning(1)));
        connection::handle_action(&mut state, action);
        assert_eq!(state.picker.as_ref().unwrap().selected, 1);
        assert_eq!(state.picker.as_ref().unwrap().items[1].label, expected);
    }
    let action = take_picker_action(&mut state, &mut editor, Key::Enter).unwrap();
    assert!(matches!(action, PickerAction::ConnectConfirmReasoning));
    connection::handle_action(&mut state, action);
    assert_eq!(state.picker.as_ref().unwrap().title, "Review connection");
    let action = take_picker_action(&mut state, &mut editor, Key::Escape).unwrap();
    assert!(matches!(
        action,
        PickerAction::ConnectBack(ConnectStep::Reasoning)
    ));
    connection::handle_action(&mut state, action);
    assert_eq!(state.picker.as_ref().unwrap().items[1].label, "[x] low");

    connection::handle_action(
        &mut state,
        PickerAction::ApplyConnect {
            field: ConnectEditField::Model,
            value: "different".into(),
        },
    );
    // An unrecognized model is treated as supporting every level; clearing
    // every box is the explicit "send no effort" choice.
    assert!(
        state
            .picker
            .as_ref()
            .unwrap()
            .items
            .iter()
            .all(|item| item.label.starts_with("[x]"))
    );
    for index in 0..crate::config::REASONING_EFFORTS.len() {
        let action = take_picker_action(&mut state, &mut editor, Key::Char(' ')).unwrap();
        connection::handle_action(&mut state, action);
        if index + 1 < crate::config::REASONING_EFFORTS.len() {
            take_picker_action(&mut state, &mut editor, Key::Down);
        }
    }
    assert!(
        state
            .picker
            .as_ref()
            .unwrap()
            .items
            .iter()
            .all(|item| item.label.starts_with("[ ]"))
    );
    let action = take_picker_action(&mut state, &mut editor, Key::Enter).unwrap();
    connection::handle_action(&mut state, action);
    assert_eq!(state.picker.as_ref().unwrap().title, "Review connection");
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
        selection_color: UiColor::WHITE,
        status_bar: Default::default(),
        status_bar_draft: None,
        color_preview: None,
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        bell: false,
        scroll_bar_idle_ticks: 0,
        scroll_geometry: None,
        scroll_bar_drag: None,
        transcript_row_entries: Vec::new(),
        tool_click_press: None,
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
        queue_paused: false,
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
                    location: None,
                },
            }],
            editing: None,
            parent: None,
        }),
        connection: None,
        model_refresh: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
        background_processes: crate::background::BackgroundProcessManager::default(),
        background_active_count: 0,
        process_view: None,
        git_view: None,
        git_job: None,
        git_init: None,
        render_cache: crate::tui::render::RenderCache::default(),
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
            location: None
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
        selection_color: UiColor::WHITE,
        status_bar: Default::default(),
        status_bar_draft: None,
        color_preview: None,
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        bell: false,
        scroll_bar_idle_ticks: 0,
        scroll_geometry: None,
        scroll_bar_drag: None,
        transcript_row_entries: Vec::new(),
        tool_click_press: None,
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
        queue_paused: false,
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
                    location: None,
                },
            }],
            editing: None,
            parent: None,
        }),
        connection: None,
        model_refresh: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
        background_processes: crate::background::BackgroundProcessManager::default(),
        background_active_count: 0,
        process_view: None,
        git_view: None,
        git_job: None,
        git_init: None,
        render_cache: crate::tui::render::RenderCache::default(),
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
fn settings_picker_categories_and_items_keep_their_action_contracts() {
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
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let dirs = config.session_dirs(&cwd);
    let session = crate::session::Session::create(&dirs.project, &cwd, "test")
        .expect("test session should be created");
    let agent = Agent::new(config, "test".into(), session, Vec::new());

    let picker = settings_picker(&agent);
    assert_eq!(picker.items.len(), 8);
    assert_eq!(
        picker.items[SettingsCategory::Providers.index()].label,
        "Providers"
    );

    let interface = settings_category_picker(&agent, SettingsCategory::Interface, 0);
    let reasoning = super::picker::settings_item_index(
        SettingsCategory::Interface,
        SettingsItem::ReasoningDisplay,
    );
    let accent =
        super::picker::settings_item_index(SettingsCategory::Interface, SettingsItem::AccentColor);
    let scroll =
        super::picker::settings_item_index(SettingsCategory::Interface, SettingsItem::ScrollBar);
    let auto_hide = super::picker::settings_item_index(
        SettingsCategory::Interface,
        SettingsItem::ScrollBarAutoHide,
    );
    assert_eq!(interface.items[reasoning].label, "Reasoning display");
    assert_eq!(interface.items[accent].label, "Accent color");
    assert_eq!(interface.items[scroll].label, "Scroll bar");
    assert!(matches!(
        interface.items[scroll].action,
        PickerAction::SetScrollBar(false)
    ));
    assert_eq!(interface.items[auto_hide].label, "Auto-hide scroll bar");
    assert!(matches!(
        interface.items[auto_hide].action,
        PickerAction::SetScrollBarAutoHide(false)
    ));
    let context = settings_category_picker(&agent, SettingsCategory::Context, 0);
    assert_eq!(context.items[0].label, "Automatic compaction");
    let web = settings_category_picker(&agent, SettingsCategory::Web, 0);
    assert_eq!(web.items.len(), 5);
    assert_eq!(web.items[0].label, "Web browsing");
    assert_eq!(web.items[1].label, "Search provider");
    assert_eq!(web.items[1].description, "DuckDuckGo · Enter to choose");
    assert!(web.items[3].description.contains("Enter"));
    assert!(matches!(
        web.items[0].action,
        PickerAction::SetWebBrowsing(true)
    ));
    assert!(matches!(
        web.items[1].action,
        PickerAction::OpenWebSearchProviders
    ));
    assert!(matches!(
        web.items[3].action,
        PickerAction::EditSecretSetting { .. }
    ));
    let advanced = settings_category_picker(&agent, SettingsCategory::Advanced, 0);
    assert_eq!(advanced.items[0].label, "Reload configuration");

    drop(agent);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn web_search_provider_picker_lists_supported_providers_and_selects_current() {
    let picker = web_search_provider_picker(crate::config::WebSearchProvider::Firecrawl);

    assert_eq!(picker.title, "Search provider");
    assert_eq!(
        picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["DuckDuckGo", "Firecrawl", "Brave"]
    );
    assert_eq!(picker.selected, 1);
    assert!(matches!(
        picker.items[1].action,
        PickerAction::SetWebSearchProvider(crate::config::WebSearchProvider::Firecrawl)
    ));
    assert!(matches!(
        picker.parent,
        Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Web,
            selected: 1,
        })
    ));
}

#[test]
fn subagent_model_picker_inherits_and_keeps_custom_and_manual_selection_on_refresh() {
    use super::picker::{PickerAction, subagent_model_picker};
    let mut config = test_agent().config().clone();
    let inherited = subagent_model_picker(&config, "inherit");
    assert_eq!(inherited.selected, 0);
    assert!(
        matches!(&inherited.items[0].action, PickerAction::ApplySetting { argument, .. } if argument == "subagent_model inherit")
    );
    assert!(matches!(
        inherited.parent,
        Some(PickerAction::OpenSettingsCategory {
            category: SettingsCategory::Subagents,
            ..
        })
    ));

    config.providers.insert(
        "local".into(),
        crate::config::ProviderConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            api: "openai-completions".into(),
            api_key: None,
            auth_header: None,
            headers: Default::default(),
            models: vec![crate::config::ModelConfig {
                id: "listed".into(),
                name: Some("Listed".into()),
                context_window: None,
                max_tokens: None,
                input: Vec::new(),
                reasoning_efforts: Vec::new(),
                compat: Default::default(),
            }],
            compat: Default::default(),
        },
    );
    let listed = subagent_model_picker(&config, "local:listed");
    assert_eq!(listed.items[listed.selected].label, "Listed");
    let custom = subagent_model_picker(&config, "local:custom");
    assert_eq!(custom.items[custom.selected].description, "local:custom");
    let refreshed =
        super::picker::refreshed_subagent_model_picker(&config, "local:custom", &custom);
    assert_eq!(
        refreshed.items[refreshed.selected].description,
        "local:custom"
    );
    let mut manual = listed;
    manual.selected = manual.items.len() - 1;
    let refreshed =
        super::picker::refreshed_subagent_model_picker(&config, "local:listed", &manual);
    assert_eq!(refreshed.selected, refreshed.items.len() - 1);
    assert!(matches!(
        refreshed.items[refreshed.selected].action,
        PickerAction::EditSetting { .. }
    ));
}

#[test]
fn subagent_model_manual_entry_and_escape_return_to_setting() {
    use super::picker::{subagent_model_picker, take_picker_action};
    let config = test_agent().config().clone();
    let mut picker = subagent_model_picker(&config, "inherit");
    picker.selected = picker.items.len() - 1;
    let mut state = test_picker_state(picker);
    let mut editor = Editor::default();
    assert!(take_picker_action(&mut state, &mut editor, Key::Enter).is_none());
    editor.paste("local:manual");
    assert!(
        matches!(take_picker_action(&mut state, &mut editor, Key::Enter), Some(PickerAction::ApplySetting { argument, location: Some(SettingsLocation { category: SettingsCategory::Subagents, item: SettingsItem::SubagentModel }) }) if argument == "subagent_model local:manual")
    );
    state.picker = Some(subagent_model_picker(&config, "inherit"));
    assert!(
        matches!(take_picker_action(&mut state, &mut editor, Key::Escape), Some(PickerAction::OpenSettingsCategory { category: SettingsCategory::Subagents, selected }) if selected == super::picker::settings_item_index(SettingsCategory::Subagents, SettingsItem::SubagentModel))
    );
}

#[test]
fn subagent_model_idle_save_reopens_the_highlighted_setting() {
    let mut agent = test_agent();
    let mut state = ViewState::from_agent(&agent);
    super::commands::activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::ApplySetting {
            argument: "subagent_model local:manual".into(),
            location: Some(SettingsLocation {
                category: SettingsCategory::Subagents,
                item: SettingsItem::SubagentModel,
            }),
        },
    );
    assert_eq!(agent.config().subagent_model, "local:manual");
    let picker = state.picker.as_ref().expect("settings reopen");
    assert_eq!(
        picker.selected,
        super::picker::settings_item_index(
            SettingsCategory::Subagents,
            SettingsItem::SubagentModel
        )
    );
    assert_eq!(picker.items[picker.selected].description, "local:manual");
}

#[test]
fn model_picker_removes_configured_models_only_after_confirmation() {
    use super::commands::activate_picker_action;
    use super::picker::{open_model_picker, remove_model_confirm, take_picker_action};

    let mut agent = test_agent();
    let home = agent.config().home_dir.clone();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.json"),
        serde_json::json!({
            "model": "test",
            "providers": {"local": {"base_url": "http://127.0.0.1:9/v1", "models": [
                {"id": "first"}, {"id": "second"}
            ]}}
        })
        .to_string(),
    )
    .unwrap();
    let mut state = ViewState::from_agent(&agent);
    activate_picker_action(&mut agent, &mut state, PickerAction::Reload);
    let mut editor = Editor::default();
    open_model_picker(&agent, &mut state, false);
    let picker = state.picker.as_mut().expect("model picker");
    assert!(picker.hint.contains("d remove"));
    let first = picker
        .items
        .iter()
        .position(|item| item.description == "local:first")
        .expect("configured model is listed");
    picker.selected = first;

    let confirm = take_picker_action(&mut state, &mut editor, Key::Char('d'))
        .expect("d asks for confirmation");
    activate_picker_action(&mut agent, &mut state, confirm);
    let picker = state.picker.as_ref().expect("confirmation picker");
    assert_eq!(picker.title, "Remove model?");
    assert_eq!(picker.selected, 0, "Cancel is the default");

    let back = take_picker_action(&mut state, &mut editor, Key::Escape).expect("escape goes back");
    activate_picker_action(&mut agent, &mut state, back);
    let picker = state.picker.as_ref().expect("model picker again");
    assert_eq!(picker.items[picker.selected].description, "local:first");
    assert_eq!(agent.config().providers["local"].models.len(), 2);

    let confirm = take_picker_action(&mut state, &mut editor, Key::Delete).unwrap();
    activate_picker_action(&mut agent, &mut state, confirm);
    assert!(take_picker_action(&mut state, &mut editor, Key::Down).is_none());
    let remove = take_picker_action(&mut state, &mut editor, Key::Enter).expect("confirm");
    activate_picker_action(&mut agent, &mut state, remove);

    let ids = agent.config().providers["local"]
        .models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["second"]);
    let picker = state.picker.as_ref().expect("model picker reopens");
    assert_eq!(picker.title, "Choose model");
    assert!(
        picker
            .items
            .iter()
            .all(|item| item.description != "local:first")
    );
    assert!(state.transcript.entries().iter().any(
        |entry| matches!(entry, Entry::Notice(text) if text.contains("Removed `local:first`"))
    ));

    let current = picker
        .items
        .iter()
        .position(|item| item.description == "test")
        .expect("current model entry");
    state.picker.as_mut().unwrap().selected = current;
    let confirm = take_picker_action(&mut state, &mut editor, Key::Char('d')).unwrap();
    activate_picker_action(&mut agent, &mut state, confirm);
    assert_eq!(state.picker.as_ref().unwrap().title, "Choose model");
    assert!(state.transcript.entries().iter().any(
        |entry| matches!(entry, Entry::Notice(text) if text.contains("not a configured provider model"))
    ));
    assert!(
        remove_model_confirm(agent.config(), "openai-codex:gpt-test", false, 0)
            .is_err_and(|message| message.contains("Codex account catalog"))
    );
}
