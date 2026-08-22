use std::time::Instant;

use crate::subagent::{RunOrigin, SubagentSnapshot, SubagentStatus, SubagentTranscriptItem};

use super::events::{Event, Key};
use super::input::{EditAction, Editor};
use super::render::{foreground_color, render_reasoning, render_user_panel, status_style};
use super::{ViewState, markdown, tool_view};

pub(super) enum SubagentView {
    Dashboard {
        selected_id: Option<String>,
        selected_index: usize,
        confirm_cancel: bool,
    },
    Takeover {
        id: String,
        scroll: TakeoverScroll,
        confirm_cancel: bool,
    },
}

/// Scroll state for the takeover transcript. While pinned, the visible
/// window holds its position in the content so streaming output below
/// does not drag it along; scrolling back down to the pin floor, where
/// the user left the tail, resumes following.
///
/// Pins hold absolute line indices, so once the bounded transcript
/// starts dropping front items, a pinned window shows progressively
/// later content and releases once it reaches the bottom.
#[derive(Default)]
pub(super) struct TakeoverScroll {
    /// Absolute first visible line while pinned; `None` follows the tail.
    pinned: Option<usize>,
    /// Window top captured when the pin started; reaching it again
    /// releases the pin even if the tail has moved on.
    pin_floor: usize,
    /// First visible line of the last rendered frame; anchors new pins.
    window_top: usize,
}

impl TakeoverScroll {
    fn pin_up(&mut self, amount: usize) {
        let top = match self.pinned {
            Some(top) => top,
            None => {
                self.pin_floor = self.window_top;
                self.window_top
            }
        };
        self.pinned = Some(top.saturating_sub(amount));
    }

    fn pin_down(&mut self, amount: usize) {
        // Already at the live tail: there is nothing further down to show.
        if let Some(top) = self.pinned {
            self.pinned = Some(top.saturating_add(amount));
        }
    }

    fn follow(&mut self) {
        self.pinned = None;
    }
}

pub(super) fn open_dashboard(state: &mut ViewState) {
    refresh(state);
    let selected_id = state
        .subagent_snapshots
        .first()
        .map(|snapshot| snapshot.id.to_string());
    state.subagent_view = Some(SubagentView::Dashboard {
        selected_id,
        selected_index: 0,
        confirm_cancel: false,
    });
    state.picker = None;
}

pub(super) fn refresh(state: &mut ViewState) {
    state.subagent_snapshots = state.subagent_manager.snapshots();
    let Some(SubagentView::Dashboard {
        selected_id,
        selected_index,
        ..
    }) = state.subagent_view.as_mut()
    else {
        return;
    };
    reconcile_selection(&state.subagent_snapshots, selected_id, selected_index);
}

fn reconcile_selection(
    snapshots: &[SubagentSnapshot],
    selected_id: &mut Option<String>,
    selected_index: &mut usize,
) {
    if snapshots.is_empty() {
        *selected_id = None;
        *selected_index = 0;
        return;
    }
    if let Some(index) = selected_id.as_deref().and_then(|id| {
        snapshots
            .iter()
            .position(|snapshot| snapshot.id.as_str() == id)
    }) {
        *selected_index = index;
    } else {
        *selected_index = (*selected_index).min(snapshots.len() - 1);
        *selected_id = Some(snapshots[*selected_index].id.to_string());
    }
}

pub(super) fn handle_event(state: &mut ViewState, editor: &mut Editor, event: Event) {
    refresh(state);
    let Some(view) = state.subagent_view.take() else {
        return;
    };
    let next_view = match view {
        SubagentView::Dashboard {
            mut selected_id,
            mut selected_index,
            mut confirm_cancel,
        } => {
            if confirm_cancel {
                match event {
                    Event::Key(Key::Enter) => {
                        if let Some(id) = selected_id.clone() {
                            let _ = state.subagent_manager.cancel(&[id], true);
                        }
                        confirm_cancel = false;
                    }
                    Event::Key(Key::Escape | Key::Ctrl('c')) => confirm_cancel = false,
                    _ => {}
                }
                Some(SubagentView::Dashboard {
                    selected_id,
                    selected_index,
                    confirm_cancel,
                })
            } else {
                match event {
                    Event::Key(Key::Escape) => {
                        editor.clear();
                        None
                    }
                    Event::Key(Key::Enter) => selected_id.map(|id| {
                        editor.clear();
                        SubagentView::Takeover {
                            id,
                            scroll: TakeoverScroll::default(),
                            confirm_cancel: false,
                        }
                    }),
                    other => {
                        match other {
                            Event::Key(Key::Up | Key::Char('k')) => {
                                selected_index = selected_index.saturating_sub(1);
                            }
                            Event::Key(Key::Down | Key::Char('j')) => {
                                selected_index = (selected_index + 1)
                                    .min(state.subagent_snapshots.len().saturating_sub(1));
                            }
                            Event::Key(Key::Char('x'))
                                if selected_snapshot(state, selected_id.as_deref())
                                    .is_some_and(|snapshot| snapshot.status.is_active()) =>
                            {
                                confirm_cancel = true;
                            }
                            _ => {}
                        }
                        if let Some(snapshot) = state.subagent_snapshots.get(selected_index) {
                            selected_id = Some(snapshot.id.to_string());
                        }
                        Some(SubagentView::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_cancel,
                        })
                    }
                }
            }
        }
        SubagentView::Takeover {
            id,
            mut scroll,
            mut confirm_cancel,
        } => {
            if confirm_cancel {
                match event {
                    Event::Key(Key::Enter) => {
                        let _ = state
                            .subagent_manager
                            .cancel(std::slice::from_ref(&id), true);
                        confirm_cancel = false;
                    }
                    Event::Key(Key::Escape | Key::Ctrl('c')) => confirm_cancel = false,
                    _ => {}
                }
                Some(SubagentView::Takeover {
                    id,
                    scroll,
                    confirm_cancel,
                })
            } else {
                match event {
                    Event::Key(Key::Escape) => {
                        editor.clear();
                        let selected_index = state
                            .subagent_snapshots
                            .iter()
                            .position(|snapshot| snapshot.id.as_str() == id)
                            .unwrap_or(0);
                        Some(SubagentView::Dashboard {
                            selected_id: Some(id),
                            selected_index,
                            confirm_cancel: false,
                        })
                    }
                    Event::Key(Key::Ctrl('c')) => {
                        if snapshot_by_id(state, &id)
                            .is_some_and(|snapshot| snapshot.status.is_active())
                        {
                            confirm_cancel = true;
                        }
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    Event::Key(Key::PageUp) | Event::MouseScroll(3) => {
                        scroll.pin_up(10);
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    // Arrows scroll the transcript; takeover is a reading
                    // view and intentionally has no editor history recall.
                    Event::Key(Key::Up) => {
                        scroll.pin_up(1);
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    Event::Key(Key::PageDown) | Event::MouseScroll(-3) => {
                        scroll.pin_down(10);
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    Event::Key(Key::Down) => {
                        scroll.pin_down(1);
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    Event::Paste(text) => {
                        editor.paste(&text);
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    Event::Key(key) => {
                        if let EditAction::Submit(message) = editor.handle_key(key) {
                            match state
                                .subagent_manager
                                .send(&id, &message, RunOrigin::PrivateUser)
                            {
                                Ok(_) => scroll.follow(),
                                Err(error) => state.notice(format!(
                                    "Could not send private subagent message: {error}"
                                )),
                            }
                        }
                        Some(SubagentView::Takeover {
                            id,
                            scroll,
                            confirm_cancel,
                        })
                    }
                    _ => Some(SubagentView::Takeover {
                        id,
                        scroll,
                        confirm_cancel,
                    }),
                }
            }
        }
    };
    state.subagent_view = next_view;
    refresh(state);
}

fn selected_snapshot<'a>(
    state: &'a ViewState,
    selected_id: Option<&str>,
) -> Option<&'a SubagentSnapshot> {
    selected_id.and_then(|id| snapshot_by_id(state, id))
}

fn snapshot_by_id<'a>(state: &'a ViewState, id: &str) -> Option<&'a SubagentSnapshot> {
    state
        .subagent_snapshots
        .iter()
        .find(|snapshot| snapshot.id.as_str() == id)
}

pub(super) fn render(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let view = state
        .subagent_view
        .as_mut()
        .expect("subagent rendering requires an active view");
    match view {
        SubagentView::Dashboard {
            selected_id,
            confirm_cancel,
            ..
        } => render_dashboard(
            &state.subagent_snapshots,
            state.accent_color,
            selected_id.as_deref(),
            *confirm_cancel,
            columns,
            rows,
        ),
        SubagentView::Takeover {
            id,
            scroll,
            confirm_cancel,
        } => render_takeover(
            &TakeoverContext {
                snapshots: &state.subagent_snapshots,
                hide_reasoning: state.hide_reasoning,
                accent_color: state.accent_color,
            },
            editor,
            id,
            scroll,
            *confirm_cancel,
            columns,
            rows,
        ),
    }
}

fn render_dashboard(
    snapshots: &[SubagentSnapshot],
    accent_color: crate::config::UiColor,
    selected_id: Option<&str>,
    confirm_cancel: bool,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(20);
    let rows = rows.max(8);
    let mut frame = vec![markdown::fit_width("\x1b[1mSubagents\x1b[0m", columns)];
    frame.push(markdown::fit_width(
        "  status  name · id  model  context  elapsed  queue",
        columns,
    ));
    frame.push(" ".repeat(columns));
    let capacity = rows.saturating_sub(5);
    let selected_index = selected_id
        .and_then(|id| {
            snapshots
                .iter()
                .position(|snapshot| snapshot.id.as_str() == id)
        })
        .unwrap_or(0);
    let start = selected_index.saturating_sub(capacity.saturating_sub(1));
    for snapshot in snapshots.iter().skip(start).take(capacity) {
        let selected = selected_id == Some(snapshot.id.as_str());
        let marker = if selected { "›" } else { " " };
        let square = match snapshot.status {
            SubagentStatus::Starting | SubagentStatus::Running => "\x1b[32m■\x1b[0m",
            SubagentStatus::Canceling => "\x1b[33m■\x1b[0m",
            SubagentStatus::Done => "\x1b[36m■\x1b[0m",
            SubagentStatus::Failed => "\x1b[31m■\x1b[0m",
        };
        let percentage = snapshot
            .context_tokens
            .saturating_mul(100)
            .checked_div(snapshot.context_window)
            .unwrap_or(0);
        let model = crate::subagent::sanitize_preview(&snapshot.model, 1024);
        let line = format!(
            "{marker} {square} {:<10} {} · {}  {}  {}% {}/{}  {}  q{}",
            snapshot.status.label(),
            snapshot.name,
            snapshot.id,
            model,
            percentage,
            snapshot.context_tokens,
            snapshot.context_window,
            format_duration(snapshot.elapsed(Instant::now())),
            snapshot.queued_messages.len()
        );
        frame.push(if selected {
            format!("\x1b[7m{}\x1b[0m", markdown::fit_width(&line, columns))
        } else {
            markdown::fit_width(&line, columns)
        });
    }
    if snapshots.is_empty() {
        frame.push(markdown::fit_width("No tracked subagents.", columns));
    }
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        rows.saturating_sub(frame.len() + 1),
    ));
    let hint = if confirm_cancel {
        "Cancel selected subagent? Enter confirms, Esc keeps it running"
    } else {
        "↑/↓ or j/k move  Enter take over  x cancel  Esc close"
    };
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(accent_color),
        markdown::fit_width(&format!(" {hint}"), columns)
    ));
    (frame, (1, 1))
}

/// Immutable render context shared by the takeover renderer.
struct TakeoverContext<'a> {
    snapshots: &'a [SubagentSnapshot],
    hide_reasoning: bool,
    accent_color: crate::config::UiColor,
}

fn render_takeover(
    context: &TakeoverContext<'_>,
    editor: &Editor,
    id: &str,
    scroll: &mut TakeoverScroll,
    confirm_cancel: bool,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(20);
    let rows = rows.max(8);
    let Some(snapshot) = context
        .snapshots
        .iter()
        .find(|snapshot| snapshot.id.as_str() == id)
    else {
        return (
            vec![markdown::fit_width(
                &format!("Subagent {id} is no longer tracked. Press Esc."),
                columns,
            )],
            (1, 1),
        );
    };
    let percentage = snapshot
        .context_tokens
        .saturating_mul(100)
        .checked_div(snapshot.context_window)
        .unwrap_or(0);
    let model = crate::subagent::sanitize_preview(&snapshot.model, 1024);
    let header = format!(
        "{} [{}] {}  {}  {}  {}% ({}/{})",
        snapshot.id,
        snapshot.status.label(),
        snapshot.name,
        model,
        format_duration(snapshot.elapsed(Instant::now())),
        percentage,
        snapshot.context_tokens,
        snapshot.context_window
    );
    let inner_width = columns.saturating_sub(2);
    let layout = editor.layout(inner_width);
    let input_line = layout
        .lines
        .get(layout.cursor_row)
        .cloned()
        .unwrap_or_default();
    let transcript_height = rows.saturating_sub(5);
    let content = render_snapshot(context.hide_reasoning, snapshot, columns);
    let max_top = content.len().saturating_sub(transcript_height);
    let top = match scroll.pinned {
        None => max_top,
        Some(pinned) => {
            let top = pinned.min(max_top);
            if top >= max_top || top >= scroll.pin_floor {
                // Back at (or past) the bottom the reader left: follow again.
                scroll.follow();
                max_top
            } else {
                top
            }
        }
    };
    scroll.window_top = top;
    let end = top
        .checked_add(transcript_height)
        .unwrap_or(content.len())
        .min(content.len());
    let visible = content[top..end].to_vec();
    let mut frame = vec![markdown::fit_width(
        &format!("\x1b[1m{header}\x1b[0m"),
        columns,
    )];
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        transcript_height.saturating_sub(visible.len()),
    ));
    frame.extend(
        visible
            .into_iter()
            .map(|line| markdown::fit_width(&line, columns)),
    );
    let accent = foreground_color(context.accent_color);
    frame.push(format!("{accent}┌{}┐\x1b[0m", "─".repeat(inner_width)));
    frame.push(format!(
        "{accent}│\x1b[0m{}{accent}│\x1b[0m",
        markdown::fit_width(&input_line, inner_width)
    ));
    frame.push(format!("{accent}└{}┘\x1b[0m", "─".repeat(inner_width)));
    let hint = if confirm_cancel {
        "Cancel this run? Enter confirms, Esc keeps it running"
    } else {
        "Enter send privately  ↑/↓ or PgUp/PgDn scroll  Ctrl+C cancel  Esc dashboard"
    };
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(context.accent_color),
        markdown::fit_width(&format!(" {hint}"), columns)
    ));
    let cursor_row = rows.saturating_sub(2);
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}

fn render_snapshot(hide_reasoning: bool, snapshot: &SubagentSnapshot, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for item in &snapshot.transcript {
        match item {
            SubagentTranscriptItem::User { text, private } => {
                if *private {
                    lines.push("\x1b[2m[private]\x1b[0m".into());
                }
                lines.extend(render_user_panel(text, width));
            }
            SubagentTranscriptItem::Assistant(text) => {
                lines.extend(markdown::render(text, width));
            }
            SubagentTranscriptItem::Reasoning { kind, text } if !hide_reasoning => {
                lines.extend(render_reasoning(*kind, text, width));
            }
            SubagentTranscriptItem::Reasoning { .. } => {}
            SubagentTranscriptItem::Tool {
                name,
                arguments,
                output,
                is_error,
            } => lines.extend(tool_view::render(
                name, arguments, output, *is_error, false, width, false,
            )),
        }
        lines.push(String::new());
    }
    if !hide_reasoning && !snapshot.live_reasoning.is_empty() {
        lines.extend(render_reasoning(
            snapshot
                .live_reasoning_kind
                .unwrap_or(crate::provider::ReasoningKind::Summary),
            &snapshot.live_reasoning,
            width,
        ));
    }
    if !snapshot.live_assistant.is_empty() {
        lines.extend(markdown::render(&snapshot.live_assistant, width));
    }
    if let Some(tool) = &snapshot.current_tool {
        lines.extend(tool_view::render(
            &tool.name,
            &tool.arguments,
            &tool.output,
            tool.is_error,
            true,
            width,
            false,
        ));
    }
    for queued in &snapshot.queued_messages {
        lines.push("\x1b[2;33m[queued]\x1b[0m".into());
        lines.extend(render_user_panel(&queued.text, width));
    }
    if !snapshot.error.is_empty() {
        lines.push("\x1b[1;31mError\x1b[0m".into());
        lines.extend(markdown::render(&snapshot.error, width));
    }
    lines
}

fn format_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;
    use crate::config::UiColor;
    use crate::subagent::{QueuedSubagentMessage, SubagentId};
    use crate::tui::transcript::Transcript;

    fn snapshot() -> SubagentSnapshot {
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(1),
            "narrow dashboard row".into(),
            "inspect the project".into(),
            "test:model".into(),
            100,
        );
        snapshot.status = SubagentStatus::Running;
        snapshot.context_tokens = 50;
        snapshot.transcript.push(SubagentTranscriptItem::Assistant(
            "live transcript content".into(),
        ));
        snapshot.queued_messages.push(QueuedSubagentMessage {
            text: "queued private message".into(),
            origin: RunOrigin::PrivateUser,
        });
        snapshot
    }

    fn state(view: SubagentView) -> ViewState {
        ViewState {
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
            queued_inputs: VecDeque::new(),
            pending_actions: VecDeque::new(),
            completions: Vec::new(),
            completion_index: 0,
            picker: None,
            subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
            subagent_snapshots: vec![snapshot()],
            subagent_view: Some(view),
        }
    }

    fn long_snapshot(items: usize) -> SubagentSnapshot {
        let mut snapshot = snapshot();
        snapshot.queued_messages.clear();
        snapshot.transcript = (1..=items)
            .map(|index| SubagentTranscriptItem::Assistant(format!("line {index:02}")))
            .collect();
        snapshot
    }

    #[test]
    fn dashboard_and_takeover_fit_narrow_terminals() {
        let mut dashboard = state(SubagentView::Dashboard {
            selected_id: Some("sa-1".into()),
            selected_index: 0,
            confirm_cancel: false,
        });
        let (dashboard_frame, dashboard_cursor) = render(&mut dashboard, &Editor::default(), 20, 8);
        assert_eq!(dashboard_frame.len(), 8);
        assert_eq!(dashboard_cursor, (1, 1));
        assert!(
            dashboard_frame
                .iter()
                .all(|line| markdown::visible_width(line) <= 20)
        );

        let mut takeover = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
        });
        let (takeover_frame, takeover_cursor) = render(&mut takeover, &Editor::default(), 20, 8);
        assert_eq!(takeover_frame.len(), 8);
        assert_eq!(takeover_cursor, (6, 4));
        assert!(
            takeover_frame
                .iter()
                .all(|line| markdown::visible_width(line) <= 20)
        );
        assert!(takeover_frame.iter().any(|line| line.contains("queued")));
    }

    #[test]
    fn takeover_arrow_keys_pin_and_step_the_transcript() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
        });
        view.subagent_snapshots = vec![long_snapshot(30)];
        let mut editor = Editor::default();
        let (tail, _) = render(&mut view, &editor, 80, 12);

        // handle_event refreshes snapshots from the (empty) test manager, so
        // restore the snapshot after each event like a live run would.
        handle_event(&mut view, &mut editor, Event::Key(Key::Up));
        view.subagent_snapshots = vec![long_snapshot(30)];
        assert!(matches!(
            &view.subagent_view,
            Some(SubagentView::Takeover { scroll, .. }) if scroll.pinned.is_some()
        ));
        let (stepped, _) = render(&mut view, &editor, 80, 12);
        assert_eq!(
            stepped[2..8],
            tail[1..7],
            "Up must shift the window one line toward earlier content"
        );

        handle_event(&mut view, &mut editor, Event::Key(Key::Down));
        view.subagent_snapshots = vec![long_snapshot(30)];
        let (resumed, _) = render(&mut view, &editor, 80, 12);
        assert_eq!(
            resumed[1..8],
            tail[1..8],
            "stepping back to the tail window must restore the same lines"
        );
        assert!(matches!(
            &view.subagent_view,
            Some(SubagentView::Takeover { scroll, .. }) if scroll.pinned.is_none()
        ));
    }

    #[test]
    fn takeover_pinned_scrolling_freezes_and_resumes_at_the_bottom() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
        });
        view.subagent_snapshots = vec![long_snapshot(30)];
        let editor = Editor::default();
        let (before, _) = render(&mut view, &editor, 80, 12);

        if let Some(SubagentView::Takeover { scroll, .. }) = view.subagent_view.as_mut() {
            scroll.pin_up(2);
        }
        let (pinned, _) = render(&mut view, &editor, 80, 12);
        assert_ne!(before[1..8], pinned[1..8], "pinning must move the window");

        // Streaming output appends below the pinned window.
        if let Some(snapshot) = view.subagent_snapshots.first_mut() {
            snapshot.transcript.push(SubagentTranscriptItem::Assistant(
                "newly generated tail".into(),
            ));
        }
        let (frozen, _) = render(&mut view, &editor, 80, 12);
        assert_eq!(
            pinned[1..8],
            frozen[1..8],
            "pinned window must hold still while output streams"
        );

        // Returning to the bottom releases the pin and follows again.
        if let Some(SubagentView::Takeover { scroll, .. }) = view.subagent_view.as_mut() {
            scroll.pin_down(2);
        }
        let (resumed, _) = render(&mut view, &editor, 80, 12);
        assert_ne!(frozen[1..8], resumed[1..8]);
        assert!(resumed.iter().any(|line| line.contains("newly generated")));
        assert!(matches!(
            &view.subagent_view,
            Some(SubagentView::Takeover { scroll, .. }) if scroll.pinned.is_none()
        ));
    }

    #[test]
    fn dashboard_selection_follows_the_agent_id_when_rows_move() {
        let first = snapshot();
        let mut second = snapshot();
        second.id = SubagentId::new(2);
        second.name = "second".into();
        let mut selected_id = Some("sa-2".to_string());
        let mut selected_index = 1;

        reconcile_selection(
            &[second.clone(), first],
            &mut selected_id,
            &mut selected_index,
        );

        assert_eq!(selected_id.as_deref(), Some("sa-2"));
        assert_eq!(selected_index, 0);
    }

    #[test]
    fn dashboard_keeps_a_late_selection_visible() {
        let mut dashboard = state(SubagentView::Dashboard {
            selected_id: Some("sa-6".into()),
            selected_index: 5,
            confirm_cancel: false,
        });
        dashboard.subagent_snapshots = (1..=6)
            .map(|sequence| {
                let mut snapshot = snapshot();
                snapshot.id = SubagentId::new(sequence);
                snapshot.name = format!("agent {sequence}");
                snapshot
            })
            .collect();

        let (frame, _) = render(&mut dashboard, &Editor::default(), 80, 8);

        assert!(frame.iter().any(|line| line.contains("sa-6")));
        assert!(!frame.iter().any(|line| line.contains("sa-1")));
    }
}
