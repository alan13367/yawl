//! Focused tests for the corresponding TUI responsibility.

use super::*;

#[test]
fn new_and_clear_are_new_session_commands() {
    assert!(is_new_session_command("new"));
    assert!(is_new_session_command("clear"));
    assert!(!is_new_session_command("compact"));
}

#[test]
fn init_accepts_no_arguments_and_keeps_the_literal_command() {
    let input = super::commands::init("").expect("bare init should run");
    assert_eq!(input.text, "/init");
    assert!(input.images.is_empty());
    assert_eq!(
        super::commands::init("replace").unwrap_err(),
        "Usage: /init"
    );
}

#[test]
fn goal_submission_expands_long_pastes_for_the_agent() {
    let mut editor = Editor::default();
    let pasted = "goal detail ".repeat(50);
    editor.paste(&pasted);
    let placeholder = editor.text();

    let (agent_argument, displayed) = prepare_goal_submission(&editor, &placeholder);

    assert_eq!(agent_argument, pasted);
    assert_eq!(displayed, pasted);
}

#[test]
fn direct_web_settings_apply_and_validate() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-web-settings-{}-{}",
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
        .expect("session should be created");
    let mut agent = Agent::new(config, "test".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    assert!(settings(&mut agent, "web_browsing on", &mut state));
    assert!(settings(
        &mut agent,
        "web_search_provider brave",
        &mut state
    ));
    assert!(settings(
        &mut agent,
        "web_fetch_max_chars 12345",
        &mut state
    ));
    assert!(settings(
        &mut agent,
        "brave_api_key test-secret",
        &mut state
    ));
    assert!(agent.config().web_browsing);
    assert_eq!(
        agent.config().web_search_provider,
        crate::config::WebSearchProvider::Brave
    );
    assert_eq!(agent.config().web_fetch_max_chars, 12_345);
    assert_eq!(agent.config().brave_api_key.as_deref(), Some("test-secret"));
    assert!(
        state.transcript.entries().is_empty(),
        "successful configuration changes should not add system notices"
    );
    assert!(!settings(
        &mut agent,
        "web_fetch_max_chars 50001",
        &mut state
    ));

    activate_picker_action(&mut agent, &mut state, PickerAction::OpenWebSearchProviders);
    assert_eq!(
        state.picker.as_ref().map(|picker| picker.items.len()),
        Some(3)
    );
    activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::SetWebSearchProvider(crate::config::WebSearchProvider::Firecrawl),
    );
    assert_eq!(
        agent.config().web_search_provider,
        crate::config::WebSearchProvider::Firecrawl
    );
    assert_eq!(state.picker.as_ref().map(|picker| picker.selected), Some(1));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn status_bar_editor_discards_drafts_and_saves_once() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-status-settings-{}-{}",
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
        .expect("session should be created");
    let mut agent = Agent::new(config, "test".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::OpenStatusBarEditor { selected: 0 },
    );
    state
        .status_bar_draft
        .as_mut()
        .expect("editor should create a draft")
        .items
        .clear();
    activate_picker_action(&mut agent, &mut state, PickerAction::CancelStatusBarEditor);
    assert!(state.status_bar_draft.is_none());
    assert_eq!(
        agent.config().status_bar,
        crate::config::StatusBarConfig::default()
    );
    assert!(!agent.config().global_config_path().exists());

    activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::OpenStatusBarEditor { selected: 0 },
    );
    let draft = state
        .status_bar_draft
        .as_mut()
        .expect("editor should create a second draft");
    draft.items.clear();
    draft.style = crate::config::StatusBarStyle::Plain;
    activate_picker_action(&mut agent, &mut state, PickerAction::SaveStatusBar);

    assert!(agent.config().status_bar.items.is_empty());
    assert!(state.status_bar.items.is_empty());
    assert!(state.status_bar_draft.is_none());
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(agent.config().global_config_path())
            .expect("saved config should be readable"),
    )
    .expect("saved config should be JSON");
    assert_eq!(saved["status_bar"]["style"], "plain");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn queue_editor_removes_a_selected_message_and_keeps_the_rest() {
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
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        bell: false,
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
        queued_inputs: ["first".into(), "second".into()].into(),
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
        render_cache: crate::tui::render::RenderCache::default(),
    };
    open_queue_picker(&mut state);
    let mut editor = Editor::default();

    let action = take_picker_action(&mut state, &mut editor, Key::Char('d'));
    let remaining = action.and_then(|action| handle_queue_picker_action(&mut state, action));

    assert!(remaining.is_none());
    assert_eq!(
        state.queued_inputs,
        std::collections::VecDeque::from(["second".into()])
    );
    assert!(state.picker.is_some());
    assert_eq!(state.activity, "removed queued message 1");

    state.queued_inputs.push_back("third".into());
    open_queue_picker(&mut state);
    assert!(take_picker_action(&mut state, &mut editor, Key::Char('e')).is_none());
    assert_eq!(editor.text(), "second");
    editor.clear();
    editor.paste("edited second");
    let action = take_picker_action(&mut state, &mut editor, Key::Enter)
        .expect("saving a queue edit returns an action");
    assert!(handle_queue_picker_action(&mut state, action).is_none());
    assert_eq!(state.queued_inputs[0].text, "edited second");

    let action = take_picker_action(&mut state, &mut editor, Key::Char('J'))
        .expect("queue reorder returns an action");
    assert!(handle_queue_picker_action(&mut state, action).is_none());
    assert_eq!(
        state.queued_inputs,
        std::collections::VecDeque::from(["third".into(), "edited second".into()])
    );

    let send = take_picker_action(&mut state, &mut editor, Key::Enter)
        .expect("Enter requests immediate delivery");
    let PickerAction::SendQueued(index) = send else {
        panic!("queue Enter should request immediate delivery");
    };
    assert!(super::commands::promote_queued(&mut state, index));
    assert_eq!(state.queued_inputs[0].text, "edited second");
}

#[test]
fn resume_picker_is_scoped_but_explicit_ids_search_other_projects() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-resume-picker-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("claude".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = crate::config::working_dir();
    let dirs = config.session_dirs(&cwd);
    let mut session = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("test session should be created");
    let session_id = session.id.clone();
    session
        .append_message(&crate::provider::Message::user("fix the bug"))
        .expect("message should append");
    let other_dir = config.sessions_dir().join("projects/other");
    std::fs::create_dir_all(&other_dir).expect("other project directory should be created");
    std::fs::write(
        other_dir.join("other-session.jsonl"),
        r#"{"type":"meta","id":"other-session","created_unix":1,"cwd":"/other","model":"claude"}"#,
    )
    .expect("other project session should be written");

    let mut agent = Agent::new(config, "claude".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    open_resume_picker(&agent, &mut state);
    let picker = state.picker.as_ref().expect("picker should be open");
    assert_eq!(picker.items.len(), 1);
    assert_eq!(picker.items[0].label, "fix the bug");
    assert_eq!(
        picker.items[0].description,
        format!("claude · {session_id}")
    );

    resume(&mut agent, "other-session", &mut state);
    assert_eq!(agent.session_id(), "other-session");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resume_picker_deletes_the_selected_session() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-resume-delete-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("claude".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = crate::config::working_dir();
    let dirs = config.session_dirs(&cwd);
    let mut keep = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("keep session should be created");
    keep.append_message(&crate::provider::Message::user("keep me"))
        .expect("message should append");
    let keep_id = keep.id.clone();
    drop(keep);
    let mut remove = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("remove session should be created");
    let remove_id = remove.id.clone();
    remove
        .append_message(&crate::provider::Message::user("delete me"))
        .expect("message should append");
    drop(remove);

    let active = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("active session should be created");
    let mut agent = Agent::new(config, "claude".into(), active, Vec::new());
    let mut state = ViewState::from_agent(&agent);
    let mut editor = Editor::default();

    open_resume_picker(&agent, &mut state);
    assert_eq!(state.picker.as_ref().map(|p| p.items.len()), Some(2));
    // Newest message-bearing session is first.
    assert!(take_picker_action(&mut state, &mut editor, Key::Char('d')).is_none());
    let confirm = state
        .picker
        .as_ref()
        .expect("delete confirmation should open");
    assert_eq!(confirm.title, "Delete session?");
    assert_eq!(confirm.selected, 0);
    assert!(matches!(
        confirm.items[1].action,
        PickerAction::DeleteSession(ref id) if id == &remove_id
    ));

    let cancel = take_picker_action(&mut state, &mut editor, Key::Escape)
        .expect("escape should return to the resume picker");
    assert!(matches!(cancel, PickerAction::OpenResume { selected: 0 }));
    activate_picker_action(&mut agent, &mut state, cancel);
    assert_eq!(state.picker.as_ref().map(|p| p.items.len()), Some(2));

    assert!(take_picker_action(&mut state, &mut editor, Key::Char('d')).is_none());
    assert!(take_picker_action(&mut state, &mut editor, Key::Down).is_none());
    let confirmed = take_picker_action(&mut state, &mut editor, Key::Enter)
        .expect("enter on Delete should confirm");
    assert!(matches!(
        confirmed,
        PickerAction::DeleteSession(ref id) if id == &remove_id
    ));
    activate_picker_action(&mut agent, &mut state, confirmed);

    assert!(!dirs.project.join(format!("{remove_id}.jsonl")).exists());
    assert!(dirs.project.join(format!("{keep_id}.jsonl")).exists());
    let picker = state.picker.as_ref().expect("picker should stay open");
    assert_eq!(picker.items.len(), 1);
    assert!(picker.items[0].description.contains(&keep_id));
    assert!(
        state
            .transcript
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Notice(text) if text.contains("Deleted session")))
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn help_lists_undo_and_copy_commands() {
    assert!(HELP.contains("/undo"));
    assert!(HELP.contains("/copy"));
    assert!(HELP.contains("/copy-all"));
    assert!(HELP.contains("/usage"));
    assert!(HELP.contains("/ps"));
    assert!(HELP.contains("/reasoning"));
    assert!(HELP.contains("/hotkeys"));
    assert!(HELP.contains("/diff"));
    assert!(HELP.contains("/init"));
    assert!(HELP.contains("| Command | Description |"));

    let rendered = crate::tui::markdown::render(HELP, 80);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains('┌') && line.contains('┬'))
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains('└') && line.contains('┴'))
    );
    assert_eq!(rendered.iter().filter(|line| line.contains('┼')).count(), 5);
}

#[test]
fn hotkeys_lists_the_key_reference_sections() {
    for fragment in [
        "Input",
        "Ctrl+G",
        "Editing",
        "Ctrl+W",
        "Transcript",
        "Ctrl+F",
        "Viewer",
        "Questions",
        "Queue",
        "K` / `J",
        "/ps",
        "/subagents",
        "| Area | Key | Action |",
        "| --- | --- | --- |",
    ] {
        assert!(HOTKEYS.contains(fragment), "HOTKEYS is missing {fragment}");
    }
    assert!(!HOTKEYS.contains("Bell"));

    let rendered = crate::tui::markdown::render(HOTKEYS, 80);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains('┌') && line.contains('┬'))
    );
    assert_eq!(rendered.iter().filter(|line| line.contains('┼')).count(), 9);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains('└') && line.contains('┴'))
    );
}

#[test]
fn bell_setting_round_trips_through_settings_and_picker() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-bell-settings-{}-{}",
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
        .expect("session should be created");
    let mut agent = Agent::new(config, "test".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);
    assert!(state.bell);

    assert!(settings(&mut agent, "bell off", &mut state));
    assert!(!agent.config().bell);
    assert!(!state.bell);
    assert!(!settings(&mut agent, "bell maybe", &mut state));

    activate_picker_action(&mut agent, &mut state, PickerAction::SetBell(true));
    assert!(agent.config().bell);
    assert!(state.bell);
    let picker = state.picker.as_ref().expect("interface picker reopens");
    assert_eq!(picker.title, "Settings · Interface");
    assert!(picker.items.iter().any(|item| item.label == "Bell"));

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn show_diff_explains_when_nothing_was_tracked() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-diff-empty-{}-{}",
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
        .expect("session should be created");
    let agent = Agent::new(config, "test".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    show_diff(&agent, &mut state);

    assert!(
        state.transcript.entries().iter().any(|entry| matches!(
            entry,
            Entry::Notice(text) if text.contains("No file changes recorded")
        )),
        "expected the no-changes notice"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn show_diff_renders_modified_created_and_deleted_files_from_disk() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-diff-files-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work dir should be created");
    let modified = work.join("modified.txt");
    let created = work.join("created.txt");
    let deleted = work.join("deleted.txt");
    let unchanged = work.join("unchanged.txt");
    let binary = work.join("binary.bin");
    std::fs::write(&modified, "after").expect("modified file should be written");
    std::fs::write(&created, "new content").expect("created file should be written");
    std::fs::write(&unchanged, "same").expect("unchanged file should be written");
    std::fs::write(&binary, [0xff, 0xfe]).expect("binary file should be written");
    // `deleted` stays missing on disk so it reads as a removal.

    let files = vec![
        crate::checkpoint::TouchedFile {
            path: modified.to_string_lossy().into_owned(),
            existed: true,
            previous: Some(b"before".to_vec()),
        },
        crate::checkpoint::TouchedFile {
            path: created.to_string_lossy().into_owned(),
            existed: false,
            previous: None,
        },
        crate::checkpoint::TouchedFile {
            path: deleted.to_string_lossy().into_owned(),
            existed: true,
            previous: Some(b"gone".to_vec()),
        },
        crate::checkpoint::TouchedFile {
            path: unchanged.to_string_lossy().into_owned(),
            existed: true,
            previous: Some(b"same".to_vec()),
        },
        crate::checkpoint::TouchedFile {
            path: binary.to_string_lossy().into_owned(),
            existed: true,
            previous: Some(b"old".to_vec()),
        },
    ];

    let config = Config {
        model: Some("test".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let dirs = config.session_dirs(&cwd);
    let session = crate::session::Session::create(&dirs.project, &cwd, "test")
        .expect("session should be created");
    let agent = Agent::new(config, "test".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    super::commands::show_diff_files(&mut state, &work, &files);

    let diffs: Vec<(&String, String)> = state
        .transcript
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            Entry::Diff { path, .. } => Some((path, entry.copy_text())),
            _ => None,
        })
        .collect();
    assert_eq!(diffs.len(), 3, "expected cards for {diffs:?}");
    assert!(
        diffs
            .iter()
            .any(|(path, _)| path.as_str() == "modified.txt")
    );
    assert!(diffs.iter().any(|(path, _)| path.as_str() == "created.txt"));
    assert!(diffs.iter().any(|(path, _)| path.as_str() == "deleted.txt"));
    assert!(!diffs.iter().any(|(path, _)| path.contains("unchanged")));
    assert!(!diffs.iter().any(|(path, _)| path.contains("binary")));

    let modified_text = diffs
        .iter()
        .find(|(path, _)| path.as_str() == "modified.txt")
        .map(|(_, text)| text)
        .expect("modified card");
    assert!(modified_text.contains("- before"), "{modified_text}");
    assert!(modified_text.contains("+ after"), "{modified_text}");

    let deleted_text = diffs
        .iter()
        .find(|(path, _)| path.as_str() == "deleted.txt")
        .map(|(_, text)| text)
        .expect("deleted card");
    assert!(deleted_text.contains("- gone"), "{deleted_text}");

    let created_text = diffs
        .iter()
        .find(|(path, _)| path.as_str() == "created.txt")
        .map(|(_, text)| text)
        .expect("created card");
    assert!(created_text.contains("+ new content"), "{created_text}");

    assert!(
        state.transcript.entries().iter().any(|entry| matches!(
            entry,
            Entry::Notice(text) if text.contains("binary.bin")
        )),
        "expected the non-UTF-8 file in the skipped notice"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn reasoning_command_shows_model_levels_and_sets_them_directly() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-reasoning-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("openai-codex:gpt-5.4".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let dirs = config.session_dirs(&cwd);
    let session = crate::session::Session::create(&dirs.project, &cwd, "openai-codex:gpt-5.4")
        .expect("session should be created");
    let mut agent = Agent::new(config, "openai-codex:gpt-5.4".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    reasoning(&mut agent, "", &mut state);
    let picker = state.picker.as_ref().expect("reasoning picker should open");
    assert!(picker.title.contains("gpt-5.4"));
    assert_eq!(
        picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Provider default",
            "Minimal",
            "Low",
            "Medium",
            "High",
            "Xhigh"
        ]
    );

    state.picker = None;
    reasoning(&mut agent, "High", &mut state);
    assert_eq!(agent.config().reasoning_effort.as_deref(), Some("high"));
    assert_eq!(state.reasoning_effort.as_deref(), Some("high"));

    reasoning(&mut agent, "max", &mut state);
    assert_eq!(
        agent.config().reasoning_effort.as_deref(),
        Some("high"),
        "an unsupported level must not change the effort"
    );
    assert!(
        state
            .transcript
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Notice(text) if text.contains("supports default")))
    );

    reasoning(&mut agent, "default", &mut state);
    assert!(agent.config().reasoning_effort.is_none());
    assert!(state.reasoning_effort.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reasoning_status_only_shows_an_effort_supported_by_the_active_model() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-reasoning-status-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("claude".into()),
        reasoning_effort: Some("high".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let dirs = config.session_dirs(&cwd);
    let session = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("session should be created");
    let mut agent = Agent::new(config, "claude".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    assert!(state.reasoning_effort.is_none());
    assert_eq!(agent.config().reasoning_effort.as_deref(), Some("high"));

    activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::SwitchModel("openai-codex:gpt-5.4".into()),
    );
    assert_eq!(state.reasoning_effort.as_deref(), Some("high"));

    activate_picker_action(
        &mut agent,
        &mut state,
        PickerAction::SwitchModel("claude".into()),
    );
    assert!(state.reasoning_effort.is_none());
    assert_eq!(agent.config().reasoning_effort.as_deref(), Some("high"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reasoning_command_reports_models_without_levels() {
    let root = std::env::temp_dir().join(format!(
        "yawl-tui-reasoning-none-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let config = Config {
        model: Some("claude".into()),
        home_dir: root.join("home/.yawl"),
        project_dir: root.join("project/.yawl"),
        ..Config::test_default()
    };
    let cwd = root.join("project");
    let dirs = config.session_dirs(&cwd);
    let session = crate::session::Session::create(&dirs.project, &cwd, "claude")
        .expect("session should be created");
    let mut agent = Agent::new(config, "claude".into(), session, Vec::new());
    let mut state = ViewState::from_agent(&agent);

    reasoning(&mut agent, "", &mut state);
    assert!(state.picker.is_none());
    reasoning(&mut agent, "high", &mut state);
    assert!(agent.config().reasoning_effort.is_none());
    assert!(
        state
            .transcript
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Notice(text) if text.contains("does not expose reasoning levels")))
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn last_assistant_reply_skips_empty_tool_only_messages() {
    let messages = [
        crate::provider::Message::user("hi"),
        crate::provider::Message::assistant(
            String::new(),
            vec![crate::provider::ToolCall {
                id: "1".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        ),
        crate::provider::Message::assistant("final answer".into(), vec![]),
    ];
    assert_eq!(last_assistant_reply(&messages, None), Some("final answer"));
    assert_eq!(
        last_assistant_reply(&messages, Some("streaming")),
        Some("streaming")
    );
    assert_eq!(last_assistant_reply(&[], None), None);
}

#[test]
fn format_copy_all_keeps_user_and_assistant_and_drops_tools_and_reasoning() {
    let mut assistant = crate::provider::Message::assistant(
        "done".into(),
        vec![crate::provider::ToolCall {
            id: "1".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        }],
    );
    assistant.reasoning.push(crate::provider::Reasoning {
        kind: crate::provider::ReasoningKind::Summary,
        content: "secret thoughts".into(),
    });
    let summary = crate::compaction::summary_message("earlier work");
    let messages = [
        summary,
        crate::provider::Message::user("please edit"),
        assistant,
        crate::provider::Message::tool_result("1", "shell", "ok".into(), false),
        crate::provider::Message::assistant(String::new(), vec![]),
    ];
    let text = format_copy_all(&messages, None);
    assert!(text.contains("User:\n[conversation summary]"));
    assert!(text.contains("User:\nplease edit"));
    assert!(text.contains("Assistant:\ndone"));
    assert!(!text.contains("secret thoughts"));
    assert!(!text.contains("ok"));
    assert_eq!(format_copy_all(&[], None), "");
}

#[test]
fn format_copy_all_drops_a_user_turn_with_only_an_empty_assistant() {
    let messages = [
        crate::provider::Message::user("keep this"),
        crate::provider::Message::assistant("answer".into(), vec![]),
        crate::provider::Message::user("failed turn"),
        crate::provider::Message::assistant(String::new(), vec![]),
        crate::compaction::summary_message("earlier work"),
    ];
    let text = format_copy_all(&messages, None);
    assert!(text.contains("User:\nkeep this"));
    assert!(text.contains("Assistant:\nanswer"));
    assert!(!text.contains("failed turn"));
    assert!(text.contains("User:\n[conversation summary]"));
}
