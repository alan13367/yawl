//! Focused tests for the corresponding TUI responsibility.

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
        show_scroll_bar: true,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        context_tokens: 12,
        context_window: 100,
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        picker: None,
    };
    let editor = Editor::default();
    let (frame, cursor) = build_frame(&mut state, &editor, 40, 12);
    assert_eq!(frame.len(), 12);
    let status = frame
        .last()
        .expect("the frame length was asserted immediately above");
    assert!(markdown::strip_ansi(status).contains("test"));
    assert_eq!(cursor.0, 10);
    assert!(status.contains("48;2;238;238;238"));
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
        is_error: false,
        running: false,
    }];

    let rendered = render_entries(&entries, 80, false, false);
    let plain = markdown::strip_ansi(&rendered.join("\n"));
    assert!(rendered.len() <= 16, "tool used {} lines", rendered.len());
    assert!(plain.contains("$ cargo test --all-targets"));
    assert!(plain.contains("lines, Ctrl+O to expand"));
    assert!(rendered.iter().any(|line| line.contains("\x1b[48;")));
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
            is_error: false,
            running: false,
        },
    ];

    let rendered = render_entries(&entries, 40, false, false);
    let plain = rendered
        .iter()
        .map(|line| markdown::strip_ansi(line).trim_end().to_string())
        .collect::<Vec<_>>();

    assert_eq!(plain, ["Answer", "", "Thinking", "", "", "$ true", "", ""]);
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
        show_scroll_bar: true,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        context_tokens: 0,
        context_window: 100,
        activity: "sending".into(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        picker: None,
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
        show_scroll_bar: true,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        context_tokens: 0,
        context_window: 100,
        activity: "sending".into(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        picker: None,
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
        is_error: false,
    }));
    assert_eq!(state.activity, "sending");
    let loading = render_loading_state(&state, 80).expect("waiting after tools");
    assert!(markdown::strip_ansi(&loading).contains("Waiting…"));
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
        picker: None,
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
        picker: None,
    }
}

#[test]
fn scroll_bar_follows_the_accent_color_when_content_overflows() {
    let mut state = overflow_state();
    let editor = Editor::default();
    let columns = 40;
    let (frame, _) = build_frame(&mut state, &editor, columns, 12);
    let transcript_rows = &frame[..8];

    // Every transcript row carries a solid background cell in the last
    // column. The brighter thumb must remain distinct from both the muted
    // track and the dark terminal background.
    let track = "\x1b[48;2;90;90;90m";
    let thumb = "\x1b[48;2;155;155;155m";
    for line in transcript_rows {
        assert_eq!(markdown::visible_width(line), columns);
        assert!(line.ends_with(" \x1b[0m"), "{line:?}");
        assert!(line.contains(track) || line.contains(thumb), "{line:?}");
    }
    // At the bottom the thumb sits on the last transcript row.
    assert!(
        transcript_rows[7].contains(thumb),
        "{:?}",
        transcript_rows[7]
    );
    assert!(
        transcript_rows[..7].iter().all(|line| line.contains(track)),
        "{transcript_rows:?}"
    );
}

#[test]
fn scroll_bar_thumb_tracks_the_viewport_position() {
    let mut state = overflow_state();
    let editor = Editor::default();
    let track = "\x1b[48;2;90;90;90m";
    let thumb = "\x1b[48;2;155;155;155m";
    let (frame_bottom, _) = build_frame(&mut state, &editor, 40, 12);

    // At the bottom the top transcript row is track.
    assert!(frame_bottom[0].contains(track));

    // Scrolling to the top moves the thumb to the first transcript row.
    state.scroll_offset = usize::MAX;
    let (frame_top, _) = build_frame(&mut state, &editor, 40, 12);
    assert!(frame_top[0].contains(thumb));
}

#[test]
fn scroll_bar_reflows_transcript_without_dropping_the_last_column() {
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
        picker: None,
    };
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);

    assert!(!frame.join("\n").contains("\x1b[48;2;"));
    assert!(state.scroll_geometry.is_none());
}

#[test]
fn scroll_bar_does_not_overlay_an_open_picker() {
    let mut state = overflow_state();
    state.picker = Some(Picker {
        title: "Settings".into(),
        hint: String::new(),
        selected: 0,
        items: Vec::new(),
        editing: None,
    });
    let editor = Editor::default();
    let (frame, _) = build_frame(&mut state, &editor, 40, 12);

    assert!(!frame[..8].iter().any(|line| line.contains("\x1b[48;2;")));
    assert!(state.scroll_geometry.is_none());
}
