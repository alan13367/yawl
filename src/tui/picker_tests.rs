//! Focused tests for the corresponding TUI responsibility.

use super::picker::{
    SettingsCategory, SettingsItem, settings_category_picker, settings_picker,
    web_search_provider_picker,
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
fn editable_setting_stays_in_the_picker_and_submits_without_a_slash_command() {
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
        usage: crate::provider::UsageSummary::default(),
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_steers: std::collections::VecDeque::new(),
        active_goal: None,
        goal_running: false,
        enter_steers: false,
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
        usage: crate::provider::UsageSummary::default(),
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_steers: std::collections::VecDeque::new(),
        active_goal: None,
        goal_running: false,
        enter_steers: false,
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
    assert_eq!(picker.items.len(), 9);
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
    let input = settings_category_picker(&agent, SettingsCategory::Input, 0);
    let enter_steers =
        super::picker::settings_item_index(SettingsCategory::Input, SettingsItem::EnterSteers);
    assert_eq!(input.items[enter_steers].label, "Enter while busy");
    assert!(matches!(
        input.items[enter_steers].action,
        PickerAction::SetEnterSteers(true)
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
