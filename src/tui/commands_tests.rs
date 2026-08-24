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
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
        render_cache: crate::tui::render::RenderCache::default(),
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
