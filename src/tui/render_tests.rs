//! Focused tests for the corresponding TUI responsibility.

use super::render::{ImageSupport, build_frame_with_images};
use super::*;

#[test]
fn user_messages_render_in_a_padded_panel() {
    let rendered = render_entries(&[Entry::User("hello".into())], 24, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));

    assert_eq!(rendered.len(), 4);
    assert!(
        rendered[..3]
            .iter()
            .all(|line| line.starts_with(USER_BACKGROUND) && markdown::visible_width(line) == 24)
    );
    assert!(plain.contains(" hello"));
    assert!(!plain.contains("You"));
}

#[test]
fn assistant_messages_do_not_show_a_title() {
    let rendered = render_entries(&[Entry::Assistant("hello".into())], 24, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));

    assert!(plain.contains("hello"));
    assert!(!plain.contains("Yawl"));
}

#[test]
fn assistant_transcript_reflows_whole_words_when_width_changes() {
    let entries = [Entry::Assistant("hello wonderful world".into())];
    let plain_lines = |width| {
        render_entries(&entries, width, false, false)
            .into_iter()
            .map(|line| markdown::strip_ansi(&line))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
    };

    assert_eq!(plain_lines(12), ["hello", "wonderful", "world"]);
    assert_eq!(plain_lines(20), ["hello wonderful", "world"]);
}

#[test]
fn frame_keeps_input_and_status_pinned() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[crate::provider::Message::assistant(
            "hello".into(),
            Vec::new(),
        )]),
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
        context_tokens: 12,
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
        render_cache: RenderCache::default(),
    };
    let editor = Editor::default();
    let (frame, cursor) = build_frame(&mut state, &editor, 40, 12);
    assert_eq!(frame.len(), 12);
    let status = frame
        .last()
        .expect("the frame length was asserted immediately above");
    assert!(markdown::strip_ansi(status).contains("test"));
    assert_eq!(cursor.0, 10);
    assert!(
        !status.contains("48;2;"),
        "the status bar draws without a background"
    );
    assert!(
        status.contains("38;2;238;238;238"),
        "the model name uses the accent color"
    );
    assert!(
        status.contains("38;2;219;219;219"),
        "the remaining status text uses the muted accent"
    );
    assert!(frame[8].contains("38;2;238;238;238"));

    state.copy_toast_ticks = 1;
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);
    assert!(markdown::strip_ansi(&frame[1]).ends_with("│ Copied! │"));
    assert!(
        frame[..3]
            .iter()
            .all(|line| markdown::visible_width(line) == 40)
    );
    advance_ticks(&mut state);
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);
    assert!(!markdown::strip_ansi(&frame.join("\n")).contains("Copied!"));
}

#[test]
fn active_background_terminal_gets_its_own_row_above_status() {
    let mut state = empty_session_state();
    state.transcript = Transcript::from_messages(&[crate::provider::Message::assistant(
        "server setup complete".into(),
        Vec::new(),
    )]);
    state
        .background_processes
        .start(crate::background::StartSpec {
            command: "sleep 30".into(),
            name: Some("dev server".into()),
            cwd: std::env::current_dir().expect("current directory"),
            timeout: None,
        })
        .expect("start background terminal");

    let (frame, cursor) = build_frame(&mut state, &Editor::default(), 54, 12);
    let notice = markdown::strip_ansi(&frame[frame.len() - 2]);
    let styled_notice = &frame[frame.len() - 2];
    let status = markdown::strip_ansi(frame.last().expect("status row"));

    assert_eq!(frame.len(), 12);
    assert!(notice.contains("1 background terminal running  ·  /ps to view"));
    assert!(
        styled_notice.contains("38;2;116;199;213"),
        "the default notice uses cyan instead of the white accent"
    );
    assert!(status.contains("test"));
    assert!(!status.contains("background terminal"));
    assert_eq!(cursor.0, 9, "the notice row must be reserved in the layout");
    let narrow = markdown::strip_ansi(&render::render_background_process_notice(
        8,
        20,
        UiColor::WHITE,
    ));
    assert!(narrow.contains("8 bg running · /ps"));

    state.background_processes.shutdown_and_discard();
    assert!(
        advance_ticks(&mut state),
        "settlement must schedule a redraw"
    );
    let (frame, cursor) = build_frame(&mut state, &Editor::default(), 54, 12);
    assert!(!markdown::strip_ansi(&frame.join("\n")).contains("background terminal running"));
    assert_eq!(cursor.0, 10, "the transcript reclaims the notice row");
}

#[test]
fn background_terminal_color_stays_distinct_from_the_accent() {
    let cyan = UiColor::parse("cyan").expect("cyan palette color");
    let amber = UiColor::parse("yellow").expect("yellow palette color");

    let with_cyan_accent = render::render_background_process_notice(1, 60, cyan);
    let with_amber_accent = render::render_background_process_notice(1, 60, amber);
    let with_custom_cyan =
        render::render_background_process_notice(1, 60, UiColor::new(110, 195, 210));

    assert!(with_cyan_accent.contains("38;2;232;202;118"));
    assert!(!with_cyan_accent.contains("38;2;116;199;213"));
    assert!(with_amber_accent.contains("38;2;116;199;213"));
    assert!(with_custom_cyan.contains("38;2;232;202;118"));
}

#[test]
fn queued_message_has_a_visible_waiting_label() {
    let rendered = render_queued_panel("follow up", 2, 50);
    let plain = markdown::strip_ansi(&rendered.join("\n"));

    assert!(plain.contains("Queued 2 · waiting for the active response"));
    assert!(plain.contains("follow up"));
}

#[test]
fn tool_entries_are_compact_and_visually_separated() {
    let output = (1..=30)
        .map(|line| format!("output line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let entries = vec![Entry::Tool {
        name: "shell".into(),
        args: r#"{"command":"cargo test --all-targets"}"#.into(),
        output,
        images: Vec::new(),
        is_error: false,
        running: false,
        started: None,
    }];

    let rendered = render_entries(&entries, 80, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));
    assert!(rendered.len() <= 16, "tool used {} lines", rendered.len());
    assert!(plain.contains("$ cargo test --all-targets"));
    assert!(plain.contains("lines, Ctrl+O to expand"));
    assert!(rendered.iter().any(|line| line.contains("\x1b[48;")));
}

#[test]
fn image_tool_results_reserve_rows_below_their_tool_card() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0; 8]);
    png.extend_from_slice(&200_u32.to_be_bytes());
    png.extend_from_slice(&100_u32.to_be_bytes());
    let image = crate::image::encode("image/png", &png);
    let messages = [
        crate::provider::Message::assistant(
            String::new(),
            vec![crate::provider::ToolCall {
                id: "read-1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"sample.png"}"#.into(),
            }],
        ),
        crate::provider::Message::tool_result_with_images(
            "read-1",
            "read_file",
            "read image/png image from sample.png".into(),
            vec![image.clone()],
            false,
        ),
    ];
    let mut state = empty_session_state();
    state.transcript = Transcript::from_messages(&messages);

    let rendered =
        build_frame_with_images(&mut state, &Editor::default(), 80, 30, ImageSupport::Png);

    assert_eq!(rendered.images.len(), 1);
    let tool_row = rendered
        .lines
        .iter()
        .position(|line| markdown::strip_ansi(line).contains("read sample.png"))
        .expect("tool card")
        + 1;
    assert!(rendered.images[0].row > tool_row);
    assert_eq!(rendered.images[0].content.as_ref(), &image);
    assert!(!rendered.lines.join("\n").contains(&image.data));

    state.picker = Some(Picker {
        title: "Settings".into(),
        hint: String::new(),
        selected: 0,
        items: Vec::new(),
        editing: None,
        parent: None,
    });
    let with_picker =
        build_frame_with_images(&mut state, &Editor::default(), 80, 30, ImageSupport::Png);
    assert!(with_picker.images.is_empty());
    state.picker = None;

    let without_preview =
        build_frame_with_images(&mut state, &Editor::default(), 80, 30, ImageSupport::None);
    assert!(without_preview.images.is_empty());
}

#[test]
fn reasoning_summary_is_one_line_and_full_reasoning_is_not() {
    let summary = Entry::Reasoning {
        kind: ReasoningKind::Summary,
        content: "Inspecting\n  the request".into(),
    };
    let full = Entry::Reasoning {
        kind: ReasoningKind::Full,
        content: "First step\n\nSecond step".into(),
    };

    let summary_lines = render_entries(&[summary], 80, false, false);
    let full_lines = render_entries(&[full], 80, false, false);

    assert_eq!(summary_lines.len(), 2);
    let summary_text = markdown::strip_ansi(&summary_lines[0]);
    assert!(summary_text.contains("Inspecting the request"));
    assert!(!summary_text.contains("Reasoning"));
    assert!(full_lines.len() > summary_lines.len());
    assert!(!markdown::strip_ansi(&full_lines.join("\n")).contains("Reasoning"));
}

#[test]
fn codex_reasoning_summary_parts_render_on_separate_lines() {
    let summary = Entry::Reasoning {
        kind: ReasoningKind::Summary,
        content: "**Planning the change**\n\n**Delegating inspection**".into(),
    };

    let rendered = render_entries(&[summary], 80, false, false);
    let visible = rendered
        .iter()
        .map(|line| markdown::strip_ansi(line).trim_end().to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    assert_eq!(visible, ["Planning the change", "Delegating inspection"]);
}

#[test]
fn hidden_reasoning_is_removed_from_the_transcript() {
    let reasoning = Entry::Reasoning {
        kind: ReasoningKind::Full,
        content: "private thought".into(),
    };

    assert!(render_entries(&[reasoning], 80, false, true).is_empty());
}

#[test]
fn reasoning_has_one_blank_line_on_each_side() {
    let entries = vec![
        Entry::Assistant("Answer\n\n".into()),
        Entry::Reasoning {
            kind: ReasoningKind::Full,
            content: "\nThinking\n\n".into(),
        },
        Entry::Tool {
            name: "shell".into(),
            args: r#"{"command":"true"}"#.into(),
            output: String::new(),
            images: Vec::new(),
            is_error: false,
            running: false,
            started: None,
        },
    ];

    let rendered = render_entries(&entries, 40, false, false);
    let plain = rendered
        .iter()
        .map(|line| markdown::strip_ansi(line).trim_end().to_string())
        .collect::<Vec<_>>();

    assert_eq!(plain, ["Answer", "", "Thinking", "", "", " $ true", "", ""]);
}

#[test]
fn loading_state_appears_under_user_prompt_and_animates() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[crate::provider::Message::user("hello")]),
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
        activity: "sending".into(),
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
        render_cache: RenderCache::default(),
    };

    let loading = render_loading_state(&state, 80).expect("loading state should be present");
    assert!(markdown::strip_ansi(&loading).contains("⠋ Waiting…"));

    advance_ticks(&mut state);
    let loading = render_loading_state(&state, 80).expect("loading state should animate");
    assert!(markdown::strip_ansi(&loading).contains("⠙ Waiting…"));

    // When assistant text arrives, loading state disappears
    state.apply(Update::Transcript(TranscriptEvent::TextDelta(
        "Hello!".into(),
    )));
    assert!(render_loading_state(&state, 80).is_none());
}

#[test]
fn loading_state_persists_during_hidden_reasoning_and_after_finished_tools() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[crate::provider::Message::user("hello")]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: true,
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
        activity: "sending".into(),
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
        render_cache: RenderCache::default(),
    };

    // Hidden reasoning delta arrives: loading state stays visible
    state.apply(Update::Transcript(TranscriptEvent::ReasoningDelta {
        kind: ReasoningKind::Full,
        text: "private thought".into(),
    }));
    assert!(render_loading_state(&state, 80).is_some());

    // Tool starts running: loading indicator is hidden while tool is active
    state.apply(Update::Transcript(TranscriptEvent::ToolStart {
        name: "shell".into(),
        args: "{}".into(),
    }));
    assert!(render_loading_state(&state, 80).is_none());

    // Tool finishes: loading indicator appears again while the next request is in flight
    state.apply(Update::Transcript(TranscriptEvent::ToolEnd {
        name: "shell".into(),
        output: "done".into(),
        images: Vec::new(),
        is_error: false,
    }));
    assert_eq!(state.activity, "sending");
    let loading = render_loading_state(&state, 80).expect("waiting after tools");
    assert!(markdown::strip_ansi(&loading).contains("Waiting…"));
}

#[test]
fn preparing_file_tool_stays_visible_after_assistant_text() {
    let mut state = overflow_state();
    state.apply(Update::from_event(crate::agent::TurnEvent::ToolPreparing {
        name: "write_file",
    }));

    let loading = render_loading_state(&state, 80)
        .expect("tool preparation should remain visible after assistant text");
    assert!(markdown::strip_ansi(&loading).contains("Preparing write…"));

    state.apply(Update::from_event(crate::agent::TurnEvent::ToolPreparing {
        name: "edit_file",
    }));
    let loading = render_loading_state(&state, 80)
        .expect("edit preparation should remain visible after assistant text");
    assert!(markdown::strip_ansi(&loading).contains("Preparing edit…"));
}

#[test]
fn skill_loading_uses_a_specific_activity_label() {
    let mut state = overflow_state();
    state.apply(Update::from_event(crate::agent::TurnEvent::ToolPreparing {
        name: "read_skill",
    }));
    let preparing = render_loading_state(&state, 80).expect("skill preparation should be visible");
    assert!(markdown::strip_ansi(&preparing).contains("Loading skill…"));

    state.apply(Update::Transcript(TranscriptEvent::ToolStart {
        name: "read_skill".into(),
        args: r#"{"name":"rust"}"#.into(),
    }));
    assert_eq!(state.activity, "loading skill");
}

#[test]
fn loading_state_ignores_status_activity() {
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
        render_cache: RenderCache::default(),
    };
    state.notice("Yawl is ready. Type /help for commands.");

    for activity in [
        "input cleared",
        "tool output expanded",
        "no queued messages",
        "change queued until the active response finishes",
    ] {
        state.activity = activity.into();
        assert!(
            render_loading_state(&state, 80).is_none(),
            "status {activity:?} should not show a spinner"
        );
    }
}

fn overflow_state() -> ViewState {
    let messages = (0..30)
        .map(|index| {
            crate::provider::Message::assistant(format!("overflow line {index}"), Vec::new())
        })
        .collect::<Vec<_>>();
    ViewState {
        transcript: Transcript::from_messages(&messages),
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
        render_cache: RenderCache::default(),
    }
}

#[test]
fn scroll_bar_draws_only_a_larger_thumb_over_the_transcript() {
    let mut state = overflow_state();
    let editor = Editor::default();
    let columns = 40;
    let (frame, _) = build_frame(&mut state, &editor, columns, 12);
    let transcript_rows = &frame[..8];

    let track = "\x1b[48;2;90;90;90m";
    let thumb = "\x1b[48;2;155;155;155m";
    for line in transcript_rows {
        assert_eq!(markdown::visible_width(line), columns);
    }
    assert!(!frame.join("\n").contains(track));
    assert!(
        transcript_rows[7].contains(thumb),
        "{:?}",
        transcript_rows[7]
    );
    assert_eq!(
        transcript_rows
            .iter()
            .filter(|line| line.contains(thumb))
            .count(),
        3,
        "the thumb should stay easy to grab even for long transcripts"
    );
}

#[test]
fn scroll_bar_overlays_reasoning_without_replacing_its_text() {
    let mut state = overflow_state();
    state.apply(Update::Transcript(TranscriptEvent::ReasoningDelta {
        kind: ReasoningKind::Full,
        text: "r".repeat(200),
    }));
    let editor = Editor::default();

    let (with_thumb, _) = build_frame(&mut state, &editor, 40, 12);
    state.scroll_bar_enabled = false;
    state.show_scroll_bar = false;
    let (without_thumb, _) = build_frame(&mut state, &editor, 40, 12);

    let plain = |frame: &[String]| {
        frame[..8]
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .collect::<Vec<_>>()
    };
    assert_eq!(plain(&with_thumb), plain(&without_thumb));
    assert!(with_thumb[..8].iter().any(|line| {
        line.contains("\x1b[2;3;38;2;148;148;158m") && line.contains("\x1b[48;2;155;155;155m")
    }));
}

#[test]
fn scroll_bar_thumb_overlays_tool_panels_without_replacing_their_content() {
    let mut state = overflow_state();
    state.apply(Update::Transcript(TranscriptEvent::ToolStart {
        name: "shell".into(),
        args: r#"{"command":"printf tool-output"}"#.into(),
    }));
    state.apply(Update::Transcript(TranscriptEvent::ToolEnd {
        name: "shell".into(),
        output: "tool-output".into(),
        images: Vec::new(),
        is_error: false,
    }));
    state.activity.clear();
    let editor = Editor::default();

    let (with_thumb, _) = build_frame(&mut state, &editor, 40, 12);
    state.scroll_bar_enabled = false;
    state.show_scroll_bar = false;
    let (without_thumb, _) = build_frame(&mut state, &editor, 40, 12);

    let plain = |frame: &[String]| markdown::strip_ansi(&frame[..8].join("\n"));
    assert_eq!(plain(&with_thumb), plain(&without_thumb));
    assert!(
        with_thumb[..8].iter().any(|line| {
            line.contains("\x1b[48;2;42;50;41m") && line.contains("\x1b[48;2;155;155;155m")
        }),
        "thumb should overlay the tool panel: {:?}",
        &with_thumb[..8]
    );
}

#[test]
fn scroll_bar_thumb_tracks_the_viewport_position() {
    let mut state = overflow_state();
    let editor = Editor::default();
    let thumb = "\x1b[48;2;155;155;155m";
    let (frame_bottom, _) = build_frame(&mut state, &editor, 40, 12);

    // At the bottom the top transcript row has no thumb.
    assert!(!frame_bottom[0].contains(thumb));

    // Scrolling to the top moves the thumb to the first transcript row.
    state.scroll_offset = usize::MAX;
    let (frame_top, _) = build_frame(&mut state, &editor, 40, 12);
    assert!(frame_top[0].contains(thumb));
}

#[test]
fn search_hit_scrolls_to_the_start_of_the_block() {
    let body = format!("unique-hit\n{}", "later line\n".repeat(40));
    let mut state = overflow_state();
    state.transcript = Transcript::from_messages(&[
        crate::provider::Message::assistant(body, Vec::new()),
        crate::provider::Message::assistant("tail-marker".into(), Vec::new()),
    ]);
    state.scroll_bar_enabled = false;
    state.show_scroll_bar = false;
    state.scroll_offset = 0;
    state.transcript.open_search();
    for character in "unique-hit".chars() {
        state.transcript.search_push(character);
    }
    let mut found = false;
    for _ in 0..100 {
        if advance_ticks(&mut state) && state.transcript.search_position().is_some() {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(found, "search worker did not publish a hit");

    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);
    let visible = markdown::strip_ansi(&frame[..7].join("\n"));

    assert!(visible.contains("unique-hit"), "{visible:?}");
    assert!(visible.contains("later line"), "{visible:?}");
    assert!(
        !visible.contains("tail-marker"),
        "search should pin the start of the hit, not the end: {visible:?}"
    );
}

#[test]
fn scroll_bar_overlay_preserves_the_last_column() {
    let mut state = overflow_state();
    let content = format!("{}Z{}", "a".repeat(39), "b".repeat(400));
    state.transcript =
        Transcript::from_messages(&[crate::provider::Message::assistant(content, Vec::new())]);
    state.scroll_offset = usize::MAX;
    let editor = Editor::default();

    let (frame, _) = build_frame(&mut state, &editor, 40, 12);
    let visible = markdown::strip_ansi(&frame[..8].join("\n"));

    assert!(visible.contains('Z'), "{visible:?}");
}

#[test]
fn scroll_bar_stays_in_the_last_column_beside_tables_with_wide_glyphs() {
    fn fixture_terminal_width(line: &str) -> usize {
        markdown::strip_ansi(line)
            .chars()
            .map(|character| unicode_width::UnicodeWidthChar::width(character).unwrap_or(0))
            .sum()
    }

    let content = "## Current peers\n\n\
        | Device | IP | Status |\n\
        | --- | --- | --- |\n\
        | macbook | 100.64.0.1 | ✅ Online |\n\
        | server | 100.64.0.2 | ⚠️ No IP assigned |\n\n\
        ## Notes\n\n\
        More content below the table so the transcript overflows.\n\n\
        Another paragraph that keeps the scroll bar visible.";
    let mut state = overflow_state();
    state.transcript = Transcript::from_messages(&[crate::provider::Message::assistant(
        content.into(),
        Vec::new(),
    )]);
    state.scroll_offset = usize::MAX;
    let editor = Editor::default();

    let (frame, _) = build_frame(&mut state, &editor, 60, 12);
    let geometry = state
        .scroll_geometry
        .expect("the fixture should overflow and draw a scroll bar");

    assert!(
        frame[..geometry.rows]
            .iter()
            .all(|line| fixture_terminal_width(line) == geometry.columns),
        "{:?}",
        frame[..geometry.rows]
            .iter()
            .map(|line| fixture_terminal_width(line))
            .collect::<Vec<_>>()
    );
}

#[test]
fn scroll_bar_setting_hides_the_bar_and_preserves_content() {
    let mut state = overflow_state();
    state.show_scroll_bar = false;
    state.scroll_bar_enabled = false;
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);

    assert!(!frame.join("\n").contains("\x1b[48;2;"));
    assert!(state.scroll_geometry.is_none());
    assert!(
        frame[..8]
            .iter()
            .all(|line| markdown::visible_width(line) == 40)
    );
}

#[test]
fn auto_hiding_scroll_bar_keeps_the_scrolled_transcript_stationary() {
    let mut state = overflow_state();
    let content = (0..400)
        .map(|index| char::from(b'a' + (index % 26) as u8))
        .collect::<String>();
    state.transcript =
        Transcript::from_messages(&[crate::provider::Message::assistant(content, Vec::new())]);
    state.scroll_bar_auto_hide = true;
    state.scroll_bar_idle_ticks = super::state::SCROLL_BAR_AUTO_HIDE_TICKS - 1;
    state.scroll_offset = 3;
    let editor = Editor::default();

    let (visible_bar, _) = build_frame(&mut state, &editor, 40, 12);
    advance_ticks(&mut state);
    assert!(!state.show_scroll_bar);
    let (hidden_bar, _) = build_frame(&mut state, &editor, 40, 12);

    let transcript_text = |frame: &[String]| {
        frame[..8]
            .iter()
            .map(|line| {
                markdown::strip_ansi(line)
                    .chars()
                    .take(39)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        transcript_text(&hidden_bar),
        transcript_text(&visible_bar),
        "hiding the bar must not move the scrolled transcript"
    );
}

#[test]
fn scroll_bar_is_absent_when_content_fits_the_transcript() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[crate::provider::Message::assistant(
            "short".into(),
            Vec::new(),
        )]),
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
        render_cache: RenderCache::default(),
    };
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);

    assert!(!frame.join("\n").contains("\x1b[48;2;"));
    assert!(state.scroll_geometry.is_none());
}

#[test]
fn scroll_bar_does_not_overlay_an_open_picker() {
    let mut state = overflow_state();
    state.accent_color = UiColor::new(12, 34, 56);
    state.picker = Some(Picker {
        title: "Settings".into(),
        hint: String::new(),
        selected: 0,
        items: Vec::new(),
        editing: None,
        parent: None,
    });
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);

    assert!(!frame[..8].iter().any(|line| line.contains("\x1b[48;2;")));
    assert!(state.scroll_geometry.is_none());
    assert!(
        frame
            .iter()
            .any(|line| line.contains("\x1b[38;2;12;34;56m┌"))
    );
}

#[test]
fn token_counts_compact_for_the_status_bar() {
    assert_eq!(crate::tui::render::format_token_count(0), "0");
    assert_eq!(crate::tui::render::format_token_count(9_999), "9999");
    assert_eq!(crate::tui::render::format_token_count(12_340), "12.3k");
    assert_eq!(crate::tui::render::format_token_count(400_000), "400k");
    assert_eq!(crate::tui::render::format_token_count(1_000_000), "1M");
    assert_eq!(crate::tui::render::format_token_count(1_234_567), "1.2M");
}

#[test]
fn reconstructed_deferred_follow_up_replaces_frozen_cache_entries() {
    let initial_messages = [
        crate::provider::Message::user("request"),
        crate::provider::Message::assistant("initial answer".into(), Vec::new()),
    ];
    let mut state = overflow_state();
    state.transcript = Transcript::from_messages(&initial_messages);
    state.render_cache = RenderCache::default();
    state.apply(Update::Transcript(TranscriptEvent::TextDelta(
        "stale follow-up".into(),
    )));
    state.apply(Update::Transcript(TranscriptEvent::AssistantDone));
    let cached =
        state
            .render_cache
            .get_or_render(&state.transcript, false, false, UiColor::WHITE, 80);
    assert!(cached.iter().any(|line| line.contains("stale follow-up")));

    let rebuilt_messages = [
        crate::provider::Message::user("request"),
        crate::provider::Message::assistant("initial answer".into(), Vec::new()),
        crate::provider::Message::subagent_results(vec![crate::provider::SubagentResult {
            id: "sa-1".into(),
            name: "review".into(),
            status: "completed".into(),
            run_number: 1,
            content: "inserted result".into(),
        }]),
        crate::provider::Message::assistant("fresh follow-up".into(), Vec::new()),
    ];
    rebuild_transcript_after_deferred_follow_up(&mut state, &rebuilt_messages);

    let rendered = state
        .render_cache
        .get_or_render(&state.transcript, false, false, UiColor::WHITE, 80)
        .join("\n");
    let plain = markdown::strip_ansi(&rendered);
    assert!(
        plain.contains("Subagent sa-1 [completed] review"),
        "{plain:?}"
    );
    assert!(plain.contains("inserted result"), "{plain:?}");
    assert!(plain.contains("fresh follow-up"), "{plain:?}");
    assert!(!plain.contains("stale follow-up"), "{plain:?}");
}

#[test]
fn retry_reset_immediately_removes_partial_output_from_render_cache() {
    let mut cache = RenderCache::default();
    let mut transcript = Transcript::from_messages(&[crate::provider::Message::user("request")]);
    transcript.apply(TranscriptEvent::ReasoningDelta {
        kind: ReasoningKind::Summary,
        text: "partial reasoning".into(),
    });
    transcript.apply(TranscriptEvent::TextDelta("partial answer".into()));

    let partial = cache
        .get_or_render(&transcript, false, false, UiColor::WHITE, 80)
        .join("\n");
    assert!(partial.contains("partial reasoning"));
    assert!(partial.contains("partial answer"));

    transcript.apply(TranscriptEvent::RetryReset);

    let reset = cache
        .get_or_render(&transcript, false, false, UiColor::WHITE, 80)
        .join("\n");
    assert!(reset.contains("request"));
    assert!(!reset.contains("partial reasoning"), "{reset:?}");
    assert!(!reset.contains("partial answer"), "{reset:?}");
}

#[test]
fn render_cache_preserves_and_updates_incremental_entries() {
    let mut cache = RenderCache::default();
    let mut transcript =
        Transcript::from_messages(&[crate::provider::Message::user("first message")]);

    let lines1 = cache.get_or_render(&transcript, false, false, UiColor::WHITE, 80);
    assert!(lines1.iter().any(|line| line.contains("first message")));
    let len1 = lines1.len();

    // Cache hit should return identical lines
    let lines2 = cache.get_or_render(&transcript, false, false, UiColor::WHITE, 80);
    assert_eq!(lines2.len(), len1);

    // Appending a notice should incrementally extend the cache
    transcript.notice("system notice".into());
    let lines3 = cache.get_or_render(&transcript, false, false, UiColor::WHITE, 80);
    assert!(lines3.len() > len1);
    assert!(lines3.iter().any(|line| line.contains("system notice")));
    assert!(lines3.iter().any(|line| line.contains("first message")));
}

#[test]
fn command_menu_lists_every_match_and_scrolls_with_the_selection() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        show_scroll_bar: false,
        scroll_bar_enabled: false,
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
        completions: (1..=12)
            .map(|n| Completion {
                command: format!("/cmd{n:02}"),
                description: format!("Command {n}"),
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
        render_cache: RenderCache::default(),
    };
    let mut editor = Editor::default();
    editor.paste("/");

    let (frame, _) = build_frame(&mut state, &editor, 40, 20);
    let first_page = markdown::strip_ansi(&frame.join("\n"));
    assert_eq!(
        frame.iter().filter(|line| line.contains("/cmd")).count(),
        COMPLETION_MENU_ROWS
    );
    assert!(first_page.contains("/cmd01"));
    assert!(first_page.contains("/cmd06"));
    assert!(!first_page.contains("/cmd07"));
    assert!(
        frame
            .iter()
            .any(|line| line.contains("/cmd01") && line.contains("48;2;238;238;238"))
    );
    assert!(
        first_page.contains("↑ wraps to end"),
        "the top indicator advertises wrapping at the first page"
    );
    assert!(
        first_page.contains("↓ 6 more · 1/12"),
        "the bottom indicator counts the hidden matches"
    );

    state.completion_index = 11;
    let (frame, _) = build_frame(&mut state, &editor, 40, 20);
    let scrolled = markdown::strip_ansi(&frame.join("\n"));
    assert_eq!(
        frame.iter().filter(|line| line.contains("/cmd")).count(),
        COMPLETION_MENU_ROWS
    );
    assert!(scrolled.contains("/cmd12"));
    assert!(!scrolled.contains("/cmd01"));
    assert!(
        frame
            .iter()
            .any(|line| line.contains("/cmd12") && line.contains("48;2;238;238;238"))
    );
    assert!(scrolled.contains("↑ 6 more"));
    assert!(scrolled.contains("↓ wraps to start · 12/12"));

    let bottom_border = frame
        .iter()
        .position(|line| line.contains('└'))
        .expect("the input box has a bottom border");
    let top_indicator = frame
        .iter()
        .position(|line| line.contains('↑'))
        .expect("the top cycle indicator is rendered");
    let first_menu_row = frame
        .iter()
        .position(|line| line.contains("/cmd"))
        .expect("the menu is rendered");
    assert_eq!(
        top_indicator,
        bottom_border + 1,
        "the menu sits directly under the input box"
    );
    assert_eq!(first_menu_row, top_indicator + 1);
    let bottom_indicator = frame
        .iter()
        .position(|line| line.contains('↓'))
        .expect("the bottom cycle indicator is rendered");
    assert_eq!(bottom_indicator, first_menu_row + COMPLETION_MENU_ROWS);
}

#[test]
fn selection_style_keeps_text_readable_for_every_color() {
    // Every palette color is light enough to demand near-black text.
    for name in [
        "white", "gray", "red", "orange", "yellow", "green", "cyan", "blue", "purple", "pink",
    ] {
        let color = UiColor::parse(name).expect("palette names parse");
        let style = selection_style(color);
        assert!(
            style.contains("38;2;16;16;16"),
            "light palette color {name} should get dark text, got {style:?}"
        );
        assert!(style.contains(&format!(
            "48;2;{};{};{}m",
            color.red, color.green, color.blue
        )));
    }
    // Dark custom colors flip to near-white text.
    let dark = UiColor::parse("#20242c").expect("hex colors parse");
    assert!(selection_style(dark).contains("38;2;250;250;250"));
}

#[test]
fn selected_row_re_arms_the_highlight_after_embedded_resets() {
    let style = selection_style(UiColor::WHITE);
    let row = selected_row("a\x1b[0mb", &style);
    assert!(row.starts_with(&style));
    assert!(
        row.contains(&format!("\x1b[0m{style}b")),
        "an embedded reset should immediately restore the highlight"
    );
    assert!(row.ends_with("\x1b[0m"));
}

#[test]
fn mention_menu_lists_matching_files_below_the_input_box() {
    let mut state = ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        show_scroll_bar: false,
        scroll_bar_enabled: false,
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
        file_index: crate::tui::files::FileIndex::with_entries(vec![
            "src/tui/render.rs".into(),
            "src/config.rs".into(),
            "README.md".into(),
        ]),
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
        render_cache: RenderCache::default(),
    };
    let mut editor = Editor::default();
    editor.paste("look at @rend");

    let (frame, _) = build_frame(&mut state, &editor, 60, 20);
    let plain = markdown::strip_ansi(&frame.join("\n"));
    assert!(plain.contains("render.rs"));
    assert!(plain.contains("src/tui"));
    assert!(!plain.contains("config.rs"));

    let bottom_border = frame
        .iter()
        .position(|line| line.contains('└'))
        .expect("the input box has a bottom border");
    let menu_row = frame
        .iter()
        .position(|line| line.contains("render.rs") && !line.contains('│'))
        .expect("the mention menu is rendered");
    assert_eq!(menu_row, bottom_border + 1);
    assert!(
        !plain.contains('↑') && !plain.contains('↓'),
        "cycle indicators only appear when matches overflow the menu"
    );
}

#[test]
fn status_bar_shows_a_live_turn_timer_next_to_the_activity() {
    let mut state = empty_session_state();
    state.activity = "running tool".into();
    state.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(95));
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 80, 24);
    let plain = markdown::strip_ansi(&frame.join("\n"));
    assert!(
        plain.contains(" ·  1m 35s"),
        "the status bar must show the turn elapsed time; got:\n{plain}"
    );
    assert!(
        !plain.contains("running tool"),
        "activity text should not clutter the status bar; got:\n{plain}"
    );
    assert!(
        !plain.contains("tokens"),
        "token usage should stay compact; got:\n{plain}"
    );

    state.turn_started = None;
    let (frame, _) = build_frame(&mut state, &editor, 80, 24);
    let plain = markdown::strip_ansi(&frame.join("\n"));
    assert!(!plain.contains("1m 35s"));
}

#[test]
fn status_bar_names_a_running_subagent_instead_of_zero_counts() {
    let mut state = empty_session_state();
    state.reasoning_effort = Some("medium".into());
    state.context_tokens = 42_679;
    state.context_window = 272_000;
    let mut snapshot = crate::subagent::SubagentSnapshot::new(
        crate::subagent::SubagentId::new(1),
        "LucidOtter".into(),
        "default".into(),
        "audit".into(),
        "test:model".into(),
        100,
    );
    snapshot.status = crate::subagent::SubagentStatus::Running;
    state.subagent_snapshots = vec![snapshot];
    state.subagent_tokens = 182_600;
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 120, 24);
    let status = markdown::strip_ansi(frame.last().expect("status bar"));
    assert!(
        status.contains("medium  ·  15% / 272k  ·  LucidOtter  ·  182.6k child"),
        "the status bar should stay compact and name the running agent; got:\n{status}"
    );
    assert!(!status.contains("running"));
    assert!(!status.contains("failed"));
    assert!(!status.contains("tokens"));
}

#[test]
fn running_tool_entries_show_their_elapsed_time() {
    let mut transcript = Transcript::from_messages(&[]);
    transcript.apply(TranscriptEvent::ToolStart {
        name: "shell".into(),
        args: r#"{"command":"cargo build"}"#.into(),
    });
    let rendered = render_entries(transcript.entries(), 80, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));
    assert!(
        plain.contains("$ cargo build  [running 0s]"),
        "a just-started tool shows a zero elapsed timer; got:\n{plain}"
    );

    transcript.apply(TranscriptEvent::ToolEnd {
        name: "shell".into(),
        output: "ok".into(),
        images: Vec::new(),
        is_error: false,
    });
    let rendered = render_entries(transcript.entries(), 80, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));
    assert!(!plain.contains("[running"));
}

fn empty_session_state() -> ViewState {
    ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        show_scroll_bar: false,
        scroll_bar_enabled: false,
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
        render_cache: RenderCache::default(),
    }
}

#[test]
fn system_notices_use_the_accent_color_instead_of_yellow() {
    let mut transcript = Transcript::from_messages(&[]);
    transcript.notice("Saved and applied.".into());
    let blue = UiColor::parse("blue").expect("palette names parse");
    let mut cache = RenderCache::default();
    let rendered = cache
        .get_or_render(&transcript, false, false, blue, 80)
        .join("\n");

    assert!(
        rendered.contains("38;2;117;169;255"),
        "the Yawl notice label should use the accent color, got {rendered:?}"
    );
    assert!(
        !rendered.contains("\x1b[1;33m"),
        "system notices should not use hardcoded yellow"
    );
    let plain = markdown::strip_ansi(&rendered);
    assert!(plain.contains("Yawl"));
    assert!(plain.contains("Saved and applied."));
}

#[test]
fn changing_accent_color_recolors_cached_notices() {
    let mut transcript = Transcript::from_messages(&[]);
    transcript.notice("hello".into());
    let mut cache = RenderCache::default();
    let white = cache
        .get_or_render(&transcript, false, false, UiColor::WHITE, 80)
        .join("\n");
    assert!(white.contains("38;2;238;238;238"));

    let blue = UiColor::parse("blue").expect("palette names parse");
    let recolored = cache
        .get_or_render(&transcript, false, false, blue, 80)
        .join("\n");
    assert!(recolored.contains("38;2;117;169;255"));
    assert!(!recolored.contains("38;2;238;238;238"));
}

#[test]
fn empty_session_shows_a_large_accent_colored_welcome() {
    let mut state = empty_session_state();
    state.spinner_tick = WELCOME_ANIMATION_TICKS;
    state.accent_color = UiColor::parse("blue").expect("palette names parse");
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 80, 24);
    let joined = frame.join("\n");
    let plain = markdown::strip_ansi(&joined);

    assert!(
        plain.contains("██    ██   █████   ██     ██  ██"),
        "fresh sessions should show the large Yawl wordmark, got {plain:?}"
    );
    assert!(plain.contains("Type /help for commands."));
    assert!(
        joined.contains("38;2;117;169;255"),
        "the welcome wordmark should use the accent color"
    );
}

#[test]
fn narrow_empty_session_falls_back_to_the_yawl_name() {
    let mut state = empty_session_state();
    state.spinner_tick = WELCOME_ANIMATION_TICKS;
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 20, 12);
    let plain = markdown::strip_ansi(&frame.join("\n"));
    assert!(plain.contains("Yawl"));
    assert!(!plain.contains("█████"));
}

#[test]
fn conversation_hides_the_welcome_banner() {
    let mut state = empty_session_state();
    state.transcript = Transcript::from_messages(&[crate::provider::Message::assistant(
        "hello".into(),
        Vec::new(),
    )]);
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 80, 24);
    let plain = markdown::strip_ansi(&frame.join("\n"));
    assert!(plain.contains("hello"));
    assert!(!plain.contains("█████"));
    assert!(!plain.contains("Type /help for commands."));
}

#[test]
fn welcome_types_the_wordmark_then_the_hint() {
    let mut state = empty_session_state();
    let editor = Editor::default();
    let blocks = |frame: &[String]| {
        markdown::strip_ansi(&frame.join("\n"))
            .chars()
            .filter(|character| *character == '█')
            .count()
    };

    let (start, _) = build_frame(&mut state, &editor, 80, 24);
    let start_plain = markdown::strip_ansi(&start.join("\n"));
    assert!(
        !start_plain.contains("██    ██   █████   ██     ██  ██"),
        "the first tick should not show the finished wordmark, got {start_plain:?}"
    );
    assert!(!start_plain.contains("Type /help for commands."));
    let start_blocks = blocks(&start);
    assert!(start_blocks > 0, "the first tick should show some of the Y");

    state.spinner_tick = 4;
    let (mid, _) = build_frame(&mut state, &editor, 80, 24);
    let mid_blocks = blocks(&mid);
    assert!(
        mid_blocks > start_blocks,
        "later ticks should reveal more of the wordmark ({start_blocks} -> {mid_blocks})"
    );
    assert!(!markdown::strip_ansi(&mid.join("\n")).contains("Type /help for commands."));

    state.spinner_tick = WELCOME_ANIMATION_TICKS;
    let (done, _) = build_frame(&mut state, &editor, 80, 24);
    let done_plain = markdown::strip_ansi(&done.join("\n"));
    assert!(done_plain.contains("██    ██   █████   ██     ██  ██"));
    assert!(done_plain.contains("Type /help for commands."));
    assert!(
        !done_plain.contains('|'),
        "the typing caret should disappear once the wordmark is finished"
    );
}
