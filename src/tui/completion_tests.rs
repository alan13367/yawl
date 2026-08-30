//! Focused tests for the corresponding TUI responsibility.

use super::*;

fn completion_state(commands: &[&str]) -> ViewState {
    ViewState {
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
        pending_steers: std::collections::VecDeque::new(),
        active_goal: None,
        goal_running: false,
        enter_steers: false,
        pending_actions: std::collections::VecDeque::new(),
        completions: commands
            .iter()
            .map(|command| Completion {
                command: (*command).into(),
                description: String::new(),
            })
            .collect(),
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
    }
}

#[test]
fn enter_runs_the_selected_completion() {
    let mut state = completion_state(&["/quit"]);
    let mut editor = Editor::default();
    editor.paste("/qui");

    assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
    assert_eq!(
        editor.handle_key(Key::Enter),
        EditAction::Submit("/quit ".into())
    );
}

#[test]
fn enter_runs_an_exact_command_name_when_longer_prefixes_also_match() {
    let mut state = completion_state(&["/copy", "/copy-all", "/compact"]);
    let mut editor = Editor::default();
    editor.paste("/copy");

    assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
    assert_eq!(
        editor.handle_key(Key::Enter),
        EditAction::Submit("/copy ".into())
    );
}

#[test]
fn enter_runs_the_first_match_when_the_typed_name_is_incomplete() {
    let mut state = completion_state(&["/clear", "/compact", "/copy", "/copy-all"]);
    let mut editor = Editor::default();
    editor.paste("/c");

    assert_eq!(state.completion_index, 0);
    assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
    assert_eq!(
        editor.handle_key(Key::Enter),
        EditAction::Submit("/clear ".into())
    );
}

#[test]
fn filtering_moves_the_cursor_back_to_the_first_match() {
    let mut state = completion_state(&["/clear", "/compact", "/copy"]);
    let mut editor = Editor::default();
    editor.paste("/");
    assert!(handle_completion_key(&mut state, &mut editor, Key::Down));
    assert_eq!(state.completion_index, 1);

    editor.paste("c");
    sync_completion_filter(&mut state, &editor);
    assert_eq!(state.completion_index, 0);
    assert_eq!(state.completion_filter.as_deref(), Some("/c"));
}

#[test]
fn matching_completions_keep_every_command() {
    let commands: Vec<String> = (1..=12).map(|n| format!("/cmd{n:02}")).collect();
    let refs: Vec<&str> = commands.iter().map(String::as_str).collect();
    let state = completion_state(&refs);
    let mut editor = Editor::default();
    editor.paste("/");

    let matches = matching_completions(&state.completions, &editor);
    assert_eq!(
        matches
            .iter()
            .map(|completion| completion.command.as_str())
            .collect::<Vec<_>>(),
        refs
    );
}

#[test]
fn up_and_down_wrap_the_full_completion_list() {
    let mut state = completion_state(&["/a", "/b", "/c"]);
    let mut editor = Editor::default();
    editor.paste("/");

    assert!(handle_completion_key(&mut state, &mut editor, Key::Up));
    assert_eq!(state.completion_index, 2);

    assert!(handle_completion_key(&mut state, &mut editor, Key::Down));
    assert_eq!(state.completion_index, 0);

    assert!(handle_completion_key(&mut state, &mut editor, Key::Down));
    assert_eq!(state.completion_index, 1);
}

fn mention_state(paths: &[&str]) -> ViewState {
    let mut state = completion_state(&[]);
    state.file_index = crate::tui::files::FileIndex::with_entries(
        paths.iter().map(|path| (*path).to_string()).collect(),
    );
    state
}

#[test]
fn enter_inserts_a_mention_tag_without_submitting() {
    let mut state = mention_state(&["src/tui/render.rs", "src/config.rs"]);
    let mut editor = Editor::default();
    editor.paste("look at @rend");

    assert!(handle_completion_key(&mut state, &mut editor, Key::Enter));
    assert_eq!(editor.text(), "look at @render.rs ");
    assert_eq!(
        editor.expand_submission(&editor.text()),
        "look at @src/tui/render.rs "
    );
}

#[test]
fn mention_menu_navigates_and_tab_completes() {
    let mut state = mention_state(&["a.rs", "b.rs"]);
    let mut editor = Editor::default();
    editor.paste("@");

    assert!(handle_completion_key(&mut state, &mut editor, Key::Down));
    assert_eq!(state.completion_index, 1);
    assert!(handle_completion_key(&mut state, &mut editor, Key::Tab));
    assert_eq!(editor.text(), "@b.rs ");
}

#[test]
fn typing_a_mention_query_resets_the_selection() {
    let mut state = mention_state(&["a.rs", "b.rs", "c.rs"]);
    let mut editor = Editor::default();
    editor.paste("@");
    assert!(handle_completion_key(&mut state, &mut editor, Key::Down));
    assert_eq!(state.completion_index, 1);

    editor.paste("c");
    sync_completion_filter(&mut state, &editor);
    assert_eq!(state.completion_index, 0);
    assert_eq!(state.completion_filter.as_deref(), Some("@c"));
}

#[test]
fn keys_pass_through_when_no_file_matches() {
    let mut state = mention_state(&["a.rs"]);
    let mut editor = Editor::default();
    editor.paste("@zzz");

    assert!(!handle_completion_key(&mut state, &mut editor, Key::Enter));
}

#[test]
fn completion_window_keeps_the_selection_visible() {
    assert_eq!(completion_window(15, 0, COMPLETION_MENU_ROWS), 0..6);
    assert_eq!(completion_window(15, 5, COMPLETION_MENU_ROWS), 0..6);
    assert_eq!(completion_window(15, 6, COMPLETION_MENU_ROWS), 1..7);
    assert_eq!(completion_window(15, 14, COMPLETION_MENU_ROWS), 9..15);
    assert_eq!(completion_window(5, 4, COMPLETION_MENU_ROWS), 0..5);
    assert_eq!(completion_window(0, 0, COMPLETION_MENU_ROWS), 0..0);
}
