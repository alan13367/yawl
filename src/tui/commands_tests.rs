//! Focused tests for the corresponding TUI responsibility.

use super::*;

#[test]
fn new_and_clear_are_new_session_commands() {
    assert!(is_new_session_command("new"));
    assert!(is_new_session_command("clear"));
    assert!(!is_new_session_command("compact"));
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
        show_scroll_bar: true,
        scroll_bar_enabled: true,
        scroll_bar_auto_hide: false,
        scroll_bar_idle_ticks: 0,
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
        completion_filter: None,
        file_index: crate::tui::files::FileIndex::default(),
        picker: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
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
    assert_eq!(state.queued_inputs[0], "edited second");

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
    assert_eq!(state.queued_inputs[0], "edited second");
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
