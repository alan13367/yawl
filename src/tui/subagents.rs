use std::sync::Arc;

use std::time::Instant;

use crate::config::UiColor;
use crate::subagent::{
    RunOrigin, RunOutcome, SubagentSnapshot, SubagentStatus, SubagentTranscriptItem,
    format_duration,
};

use super::dashboard::{
    Alignment, FieldColumn, PinnedScroll, TableChrome, reconcile_selection, render_table,
};
use super::events::{Event, Key, MouseKind};
use super::input::{EditAction, Editor};
use super::render::{
    HIDDEN_CURSOR, SPINNER_FRAMES, activity_color, composer_top_border, foreground_color,
    render_reasoning, render_thinking_tag, render_user_panel, status_style,
};
use super::{ViewState, markdown, tool_view};

/// Dark text for labels drawn on the light subagent ink.
const BADGE_TEXT: &str = "\x1b[38;2;18;18;22m";
const STEER_PENDING: &str = "\x1b[2;36m";
const STEER_DELIVERED: &str = "\x1b[36m";
const QUEUED: &str = "\x1b[2;33m";

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
        /// Tool cards and thinking blocks show their full content.
        expanded: bool,
    },
}

/// Scroll state for the takeover transcript, which follows the live tail
/// unless the reader pins it.
#[derive(Default)]
pub(super) struct TakeoverScroll {
    pin: PinnedScroll,
    /// Blank rows above short content and the transcript height in the last
    /// frame, which map clicked rows back to content lines.
    region_padding: usize,
    region_height: usize,
    /// Cell of a pending mouse press; a release on the same cell is a click.
    press: Option<(usize, usize)>,
    cache: SnapshotRenderCache,
}

impl TakeoverScroll {
    /// Wheel amounts are positive toward earlier content.
    fn wheel(&mut self, amount: i32) {
        let rows = amount.unsigned_abs() as usize;
        if amount > 0 {
            self.pin.up(rows);
        } else {
            self.pin.down(rows);
        }
    }

    /// Click-to-expand, matching the main transcript: a press and release on
    /// the same cell toggles a tool card from any row, or a thinking block
    /// from its tag row. Row 0 is the header.
    fn click(&mut self, press: (usize, usize), release: (usize, usize)) -> bool {
        if press != release {
            return false;
        }
        let Some(offset) = release
            .0
            .checked_sub(1)
            .filter(|row| *row < self.region_height)
            .and_then(|row| row.checked_sub(self.region_padding))
        else {
            return false;
        };
        self.cache.toggle_at(self.pin.window_top() + offset)
    }
}

pub(super) fn open_dashboard(state: &mut ViewState) {
    refresh(state);
    if state.subagent_snapshots.is_empty() {
        state.subagent_view = None;
        state.notice(empty_dashboard_message(state.subagents_enabled));
        return;
    }
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

fn empty_dashboard_message(subagents_enabled: bool) -> &'static str {
    if subagents_enabled {
        "No tracked subagents. The dashboard opens once the model spawns one."
    } else {
        "Subagents are off. Enable them with /settings subagents on, then ask the model to spawn one."
    }
}

fn snapshot_id(snapshot: &SubagentSnapshot) -> &str {
    snapshot.id.as_str()
}

pub(super) fn refresh(state: &mut ViewState) {
    let selected = match state.subagent_view.as_ref() {
        Some(SubagentView::Takeover { id, .. }) => Some(id.as_str()),
        _ => None,
    };
    (state.subagent_snapshots, state.subagent_tokens) =
        state.subagent_manager.display_snapshots(selected);
    let Some(SubagentView::Dashboard {
        selected_id,
        selected_index,
        ..
    }) = state.subagent_view.as_mut()
    else {
        return;
    };
    reconcile_selection(
        &state.subagent_snapshots,
        snapshot_id,
        selected_id,
        selected_index,
    );
}

pub(super) fn handle_event(state: &mut ViewState, editor: &mut Editor, event: Event) {
    let Some(view) = state.subagent_view.take() else {
        return;
    };
    // These views take every event, so ticks must drive their spinners here.
    if matches!(event, Event::Tick) {
        state.spinner_tick = state.spinner_tick.wrapping_add(1);
    }
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
                            expanded: state.tools_expanded,
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
            mut expanded,
        } => {
            if !confirm_cancel && matches!(event, Event::Key(Key::Escape)) {
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
            } else {
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
                } else {
                    handle_takeover_input(
                        state,
                        editor,
                        &id,
                        &mut scroll,
                        &mut confirm_cancel,
                        &mut expanded,
                        event,
                    );
                }
                Some(SubagentView::Takeover {
                    id,
                    scroll,
                    confirm_cancel,
                    expanded,
                })
            }
        }
    };
    state.subagent_view = next_view;
    refresh(state);
}

fn handle_takeover_input(
    state: &mut ViewState,
    editor: &mut Editor,
    id: &str,
    scroll: &mut TakeoverScroll,
    confirm_cancel: &mut bool,
    expanded: &mut bool,
    event: Event,
) {
    match event {
        Event::Key(Key::Ctrl('c')) => {
            *confirm_cancel =
                snapshot_by_id(state, id).is_some_and(|snapshot| snapshot.status.is_active());
        }
        Event::Key(Key::Ctrl('o')) => {
            *expanded = !*expanded;
            scroll.cache.clear_toggles();
        }
        Event::Key(Key::PageUp) => scroll.pin.up(10),
        // Arrows scroll the transcript; takeover is a reading view and
        // intentionally has no editor history recall.
        Event::Key(Key::Up) => scroll.pin.up(1),
        Event::Key(Key::PageDown) => scroll.pin.down(10),
        Event::Key(Key::Down) => scroll.pin.down(1),
        Event::MouseScroll(amount) => scroll.wheel(amount),
        Event::Mouse(mouse) => match mouse.kind {
            MouseKind::Press => scroll.press = Some((mouse.row, mouse.column)),
            MouseKind::Release => {
                if let Some(press) = scroll.press.take() {
                    scroll.click(press, (mouse.row, mouse.column));
                }
            }
            MouseKind::Drag | MouseKind::Move => {}
        },
        Event::Paste(text) => editor.paste(&text),
        Event::Key(key) => {
            let action = if key == Key::Tab {
                editor.queue()
            } else {
                editor.handle_key(key)
            };
            match action {
                EditAction::Submit(message) | EditAction::Steer(message) => {
                    let message = editor.expand_submission(&message.text);
                    match state.subagent_manager.steer(id, &message) {
                        Ok(_) => scroll.pin.follow(),
                        Err(error) => {
                            state.notice(format!("Could not steer the subagent: {error}"))
                        }
                    }
                }
                EditAction::Queue(message) => {
                    let message = editor.expand_submission(&message.text);
                    match state
                        .subagent_manager
                        .send(id, &message, RunOrigin::PrivateUser)
                    {
                        Ok(_) => scroll.pin.follow(),
                        Err(error) => state.notice(format!(
                            "Could not send a private message to the subagent: {error}"
                        )),
                    }
                }
                EditAction::None => {}
            }
        }
        _ => {}
    }
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
            state.selection_color,
            selected_id.as_deref(),
            *confirm_cancel,
            columns,
            rows,
        ),
        SubagentView::Takeover {
            id,
            scroll,
            confirm_cancel,
            expanded,
        } => render_takeover(
            &TakeoverContext {
                snapshots: &state.subagent_snapshots,
                hide_reasoning: state.hide_reasoning,
                expanded: *expanded,
                accent_color: state.accent_color,
                spinner_tick: state.spinner_tick,
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

fn status_square(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => "\x1b[90m■\x1b[0m",
        SubagentStatus::Starting | SubagentStatus::Running => "\x1b[32m■\x1b[0m",
        SubagentStatus::Canceling => "\x1b[33m■\x1b[0m",
        SubagentStatus::Done => "\x1b[36m■\x1b[0m",
        SubagentStatus::Failed => "\x1b[31m■\x1b[0m",
    }
}

fn render_dashboard(
    snapshots: &[SubagentSnapshot],
    accent_color: UiColor,
    selection_color: UiColor,
    selected_id: Option<&str>,
    confirm_cancel: bool,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let now = Instant::now();
    let selected = selected_id.and_then(|id| {
        snapshots
            .iter()
            .position(|snapshot| snapshot.id.as_str() == id)
    });
    let active = snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active())
        .count();
    let chrome = TableChrome {
        title: "Subagents",
        summary: super::dashboard::summary(active, snapshots.len(), "active"),
        empty: "No tracked subagents.",
        hint: if confirm_cancel {
            "Cancel selected subagent? Enter confirms, Esc keeps it running"
        } else {
            "↑↓ move · Enter take over · x cancel · Esc close"
        },
        accent: accent_color,
        selection: selection_color,
    };
    let cell = |snapshot: &SubagentSnapshot, field, selected: bool| match field {
        SubagentField::Status => format!(
            "{} {} {}",
            if selected { "›" } else { " " },
            status_square(snapshot.status),
            snapshot.status.label()
        ),
        SubagentField::Name if snapshot.agent == "default" => snapshot.name.clone(),
        SubagentField::Name => format!("{} · {}", snapshot.name, snapshot.agent),
        SubagentField::Id => snapshot.id.to_string(),
        SubagentField::Model => crate::subagent::sanitize_preview(&snapshot.model, 1024),
        SubagentField::Context => format!(
            "{}% · {}/{}",
            snapshot
                .context_tokens
                .saturating_mul(100)
                .checked_div(snapshot.context_window)
                .unwrap_or(0),
            snapshot.context_tokens,
            snapshot.context_window
        ),
        SubagentField::Elapsed => format_duration(snapshot.elapsed(now)),
        SubagentField::Queue => snapshot.queued_messages.len().to_string(),
    };
    let frame = render_table(
        snapshots,
        selected,
        chrome,
        subagent_columns,
        cell,
        columns,
        rows,
    );
    (frame, HIDDEN_CURSOR)
}

#[derive(Clone, Copy)]
enum SubagentField {
    Status,
    Name,
    Id,
    Model,
    Context,
    Elapsed,
    Queue,
}

type SubagentColumn = FieldColumn<SubagentField>;

fn subagent_columns(width: usize) -> Vec<SubagentColumn> {
    if width >= 75 {
        let mut extra = width - 75;
        let name_extra = extra.min(12);
        extra -= name_extra;
        let model_extra = extra.min(12);
        extra -= model_extra;
        let context_extra = extra.min(6);
        extra -= context_extra;
        let elapsed_extra = extra.min(1);
        extra -= elapsed_extra;
        let queue_extra = extra.min(1);
        extra -= queue_extra;
        vec![
            SubagentColumn::new(SubagentField::Status, "    STATUS", 13, Alignment::Left),
            SubagentColumn::new(
                SubagentField::Name,
                "NAME",
                14 + name_extra + extra,
                Alignment::Left,
            ),
            SubagentColumn::new(SubagentField::Id, "ID", 6, Alignment::Left),
            SubagentColumn::new(
                SubagentField::Model,
                "MODEL",
                12 + model_extra,
                Alignment::Left,
            ),
            SubagentColumn::new(
                SubagentField::Context,
                "CONTEXT",
                12 + context_extra,
                Alignment::Right,
            ),
            SubagentColumn::new(
                SubagentField::Elapsed,
                "TIME",
                7 + elapsed_extra,
                Alignment::Right,
            ),
            SubagentColumn::new(
                SubagentField::Queue,
                "QUEUE",
                5 + queue_extra,
                Alignment::Right,
            ),
        ]
    } else if width >= 56 {
        let extra = width - 56;
        let name_extra = extra.min(10);
        let model_extra = extra - name_extra;
        vec![
            SubagentColumn::new(SubagentField::Status, "    STATUS", 13, Alignment::Left),
            SubagentColumn::new(
                SubagentField::Name,
                "NAME",
                14 + name_extra,
                Alignment::Left,
            ),
            SubagentColumn::new(SubagentField::Id, "ID", 6, Alignment::Left),
            SubagentColumn::new(
                SubagentField::Model,
                "MODEL",
                12 + model_extra,
                Alignment::Left,
            ),
            SubagentColumn::new(SubagentField::Elapsed, "TIME", 7, Alignment::Right),
        ]
    } else if width >= 31 {
        vec![
            SubagentColumn::new(SubagentField::Status, "    STATUS", 13, Alignment::Left),
            SubagentColumn::new(SubagentField::Name, "NAME", width - 21, Alignment::Left),
            SubagentColumn::new(SubagentField::Id, "ID", 6, Alignment::Left),
        ]
    } else {
        let status_width = 12.min(width.saturating_sub(2));
        vec![
            SubagentColumn::new(
                SubagentField::Status,
                "    STATUS",
                status_width,
                Alignment::Left,
            ),
            SubagentColumn::new(
                SubagentField::Name,
                "NAME",
                width.saturating_sub(status_width + 1),
                Alignment::Left,
            ),
        ]
    }
}

/// Immutable render context shared by the takeover renderer.
struct TakeoverContext<'a> {
    snapshots: &'a [SubagentSnapshot],
    hide_reasoning: bool,
    expanded: bool,
    accent_color: UiColor,
    spinner_tick: usize,
}

/// Everything that changes how a transcript item renders.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ItemStyle {
    width: usize,
    hide_reasoning: bool,
    expanded: bool,
    accent: UiColor,
}

/// The takeover mirrors the main screen (transcript, live tail, composer,
/// hint line) but draws its chrome in the subagent activity color under a
/// `Subagent` header, so it never reads as the orchestrator's conversation.
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
            HIDDEN_CURSOR,
        );
    };
    let inner_width = columns.saturating_sub(2);
    let layout = editor.layout(inner_width);
    let max_input_lines = (rows / 3).max(1);
    let input_start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(max_input_lines)
        .min(layout.lines.len().saturating_sub(max_input_lines));
    let input_end = (input_start + max_input_lines).min(layout.lines.len());
    let input_lines = layout.lines.get(input_start..input_end).unwrap_or(&[]);
    let input_rows = input_lines.len().max(1);
    // The header, both composer borders, and the hint line frame the transcript.
    let transcript_height = rows.saturating_sub(input_rows + 4);
    let style = ItemStyle {
        width: columns,
        hide_reasoning: context.hide_reasoning,
        expanded: context.expanded,
        accent: context.accent_color,
    };
    scroll.cache.update(style, snapshot);
    scroll.cache.tail_thinking = shows_live_thinking(snapshot, style);
    let tail = render_snapshot_tail(
        snapshot,
        style,
        style.expanded != scroll.cache.live_toggled,
        context.spinner_tick,
    );
    let content_len = scroll.cache.lines.len() + tail.len();
    let top = scroll.pin.place(content_len, transcript_height);
    let end = top
        .checked_add(transcript_height)
        .unwrap_or(content_len)
        .min(content_len);
    let visible = (top..end)
        .map(|index| {
            if index < scroll.cache.lines.len() {
                scroll.cache.lines[index].clone()
            } else {
                tail[index - scroll.cache.lines.len()].clone()
            }
        })
        .collect::<Vec<_>>();
    scroll.region_height = transcript_height;
    scroll.region_padding = transcript_height.saturating_sub(visible.len());
    let mut frame = Vec::with_capacity(rows);
    frame.push(render_takeover_header(
        snapshot,
        context.accent_color,
        columns,
    ));
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        transcript_height.saturating_sub(visible.len()),
    ));
    frame.extend(
        visible
            .into_iter()
            .map(|line| markdown::fit_width(&line, columns)),
    );
    let ink = foreground_color(activity_color(context.accent_color));
    let label = composer_label(snapshot, inner_width);
    frame.push(format!(
        "{ink}┌{}┐\x1b[0m",
        composer_top_border(inner_width, Some(&label))
    ));
    if input_lines.is_empty() {
        frame.push(format!(
            "{ink}│\x1b[0m{}{ink}│\x1b[0m",
            " ".repeat(inner_width)
        ));
    }
    for line in input_lines {
        frame.push(format!(
            "{ink}│\x1b[0m{}{ink}│\x1b[0m",
            markdown::fit_composer_width(line, inner_width)
        ));
    }
    let active = snapshot.status.is_active();
    let bottom_border = if active && !editor.is_empty() {
        const HINT: &str = " Tab queues ";
        format!(
            "{}{HINT}",
            "─".repeat(inner_width.saturating_sub(HINT.len()))
        )
    } else {
        "─".repeat(inner_width)
    };
    frame.push(format!("{ink}└{bottom_border}┘\x1b[0m"));
    let expand = if context.expanded {
        "collapse"
    } else {
        "expand"
    };
    let hint = if confirm_cancel {
        "Cancel this run? Enter confirms, Esc keeps it running".to_string()
    } else if active {
        format!("Enter steer · Tab queue · ↑↓ scroll · Ctrl+O {expand} · Ctrl+C cancel · Esc back")
    } else {
        format!("Enter message · ↑↓ scroll · Ctrl+O {expand} · Esc back")
    };
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(context.accent_color),
        markdown::fit_width(&format!(" {hint}"), columns)
    ));
    let cursor_row = transcript_height + 3 + layout.cursor_row.saturating_sub(input_start);
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}

/// `Subagent` badge, identity, and a right-aligned state summary.
fn render_takeover_header(snapshot: &SubagentSnapshot, accent: UiColor, columns: usize) -> String {
    let ink = activity_color(accent);
    let badge = format!(
        "\x1b[1m{BADGE_TEXT}\x1b[48;2;{};{};{}m Subagent \x1b[0m",
        ink.red, ink.green, ink.blue
    );
    let name = crate::subagent::sanitize_preview(&snapshot.name, 256);
    let mut details = Vec::new();
    if snapshot.agent != "default" {
        details.push(crate::subagent::sanitize_preview(&snapshot.agent, 128));
    }
    details.push(snapshot.id.to_string());
    details.push(crate::subagent::sanitize_preview(&snapshot.model, 256));
    let left = format!(
        " {badge} {}\x1b[1m{name}\x1b[0m \x1b[2m· {}\x1b[0m",
        foreground_color(ink),
        details.join(" · ")
    );
    let (state, color) = state_label(snapshot);
    let percentage = snapshot
        .context_tokens
        .saturating_mul(100)
        .checked_div(snapshot.context_window)
        .unwrap_or(0);
    let right = format!(
        "{color}{state}\x1b[0m \x1b[2m· {} · {percentage}% ctx\x1b[0m ",
        format_duration(snapshot.elapsed(Instant::now()))
    );
    let left_width = markdown::visible_width(&left);
    let right_width = markdown::visible_width(&right);
    if left_width + right_width < columns {
        format!(
            "{left}{}{right}",
            " ".repeat(columns - left_width - right_width)
        )
    } else {
        markdown::fit_width(&left, columns)
    }
}

/// Settled children report how their latest run ended rather than `done`.
fn state_label(snapshot: &SubagentSnapshot) -> (&'static str, &'static str) {
    match (snapshot.status, snapshot.latest_outcome) {
        (SubagentStatus::Queued, _) => ("queued", "\x1b[2m"),
        (SubagentStatus::Canceling, _) => ("canceling", "\x1b[33m"),
        (status, _) if status.is_active() => (status.label(), "\x1b[32m"),
        (_, Some(RunOutcome::Completed)) => ("completed", "\x1b[32m"),
        (_, Some(RunOutcome::Interrupted)) => ("interrupted", "\x1b[33m"),
        (SubagentStatus::Failed, _) | (_, Some(RunOutcome::Failed)) => ("failed", "\x1b[31m"),
        (status, None) => (status.label(), "\x1b[36m"),
    }
}

/// Names where composer input goes: a steer while the child works, a new
/// message once it settled.
fn composer_label(snapshot: &SubagentSnapshot, width: usize) -> String {
    let verb = match snapshot.status {
        SubagentStatus::Canceling => return "Canceling".into(),
        status if status.is_active() => "Steer",
        _ => "Message",
    };
    let name = crate::subagent::sanitize_preview(&snapshot.name, 256);
    let room = width.saturating_sub(verb.len() + 6);
    if room < 4 {
        return verb.into();
    }
    format!("{verb} {}", ellipsize(&name, room))
}

fn ellipsize(text: &str, width: usize) -> String {
    if markdown::visible_width(text) <= width {
        text.to_string()
    } else {
        let kept = markdown::fit_width(text, width.saturating_sub(1));
        format!("{}…", kept.trim_end())
    }
}

#[derive(Default)]
struct SnapshotRenderCache {
    style: Option<ItemStyle>,
    entries: Vec<Arc<SubagentTranscriptItem>>,
    starts: Vec<usize>,
    lines: Vec<String>,
    /// Items clicked away from the view's default expansion. Holding the
    /// `Arc` keeps each identity unique while the toggle lasts.
    toggled: Vec<Arc<SubagentTranscriptItem>>,
    /// The streaming thinking block was clicked; the toggle moves to its
    /// transcript item once the block settles.
    live_toggled: bool,
    /// Whether the live tail opens with a thinking tag.
    tail_thinking: bool,
}

impl SnapshotRenderCache {
    fn is_toggled(&self, item: &Arc<SubagentTranscriptItem>) -> bool {
        self.toggled
            .iter()
            .any(|toggled| Arc::ptr_eq(toggled, item))
    }

    fn clear_toggles(&mut self) {
        self.toggled.clear();
        self.live_toggled = false;
    }

    /// Toggles the expandable item at content `line`. Returns false for
    /// lines that do not own a tool card or a thinking tag.
    fn toggle_at(&mut self, line: usize) -> bool {
        if line >= self.lines.len() {
            if line == self.lines.len() && self.tail_thinking {
                self.live_toggled = !self.live_toggled;
                return true;
            }
            return false;
        }
        let index = self
            .starts
            .partition_point(|start| *start <= line)
            .saturating_sub(1);
        let Some(item) = self.entries.get(index) else {
            return false;
        };
        let toggleable = match item.as_ref() {
            SubagentTranscriptItem::Tool { .. } => true,
            SubagentTranscriptItem::Reasoning { .. } => self.starts[index] == line,
            _ => false,
        };
        if !toggleable {
            return false;
        }
        if let Some(position) = self
            .toggled
            .iter()
            .position(|toggled| Arc::ptr_eq(toggled, item))
        {
            self.toggled.remove(position);
        } else {
            self.toggled.push(Arc::clone(item));
        }
        // Render this item and everything after it again on the next frame.
        self.lines.truncate(self.starts[index]);
        self.entries.truncate(index);
        self.starts.truncate(index);
        true
    }

    fn update(&mut self, style: ItemStyle, snapshot: &SubagentSnapshot) {
        let same_style = self.style == Some(style);
        // The bounded history can evict its front. Retain the rendering of
        // surviving entries instead of reparsing the full transcript.
        if same_style
            && let Some(first) = snapshot.transcript.first()
            && let Some(offset) = self
                .entries
                .iter()
                .position(|entry| Arc::ptr_eq(entry, first))
            && offset > 0
        {
            let removed_lines = self.starts[offset];
            self.lines.drain(..removed_lines);
            self.entries.drain(..offset);
            self.starts.drain(..offset);
            for start in &mut self.starts {
                *start -= removed_lines;
            }
            let entries = &self.entries;
            self.toggled
                .retain(|toggled| entries.iter().any(|entry| Arc::ptr_eq(entry, toggled)));
        }
        let common = if same_style {
            self.entries
                .iter()
                .zip(&snapshot.transcript)
                .take_while(|(old, new)| Arc::ptr_eq(old, new))
                .count()
        } else {
            0
        };
        self.lines
            .truncate(self.starts.get(common).copied().unwrap_or(self.lines.len()));
        if common == 0 {
            self.lines.clear();
        }
        self.entries.truncate(common);
        self.starts.truncate(common);
        self.style = Some(style);
        for item in &snapshot.transcript[common..] {
            if same_style
                && self.live_toggled
                && matches!(item.as_ref(), SubagentTranscriptItem::Reasoning { .. })
            {
                self.live_toggled = false;
                self.toggled.push(Arc::clone(item));
            }
            let expanded = style.expanded != self.is_toggled(item);
            self.starts.push(self.lines.len());
            self.lines
                .extend(render_snapshot_item(item, style, expanded));
            self.entries.push(Arc::clone(item));
        }
        if snapshot.live_reasoning.is_empty() {
            self.live_toggled = false;
        }
    }
}

/// Who wrote a message the child received.
fn sender(private: bool) -> &'static str {
    if private { "you" } else { "orchestrator" }
}

fn render_snapshot_item(
    item: &SubagentTranscriptItem,
    style: ItemStyle,
    expanded: bool,
) -> Vec<String> {
    let width = style.width;
    let mut lines = Vec::new();
    match item {
        SubagentTranscriptItem::User { text, private } => {
            let label = if *private {
                "From you · private"
            } else {
                "From orchestrator"
            };
            lines.push(markdown::fit_width(
                &format!("\x1b[2m{label}\x1b[0m"),
                width,
            ));
            lines.extend(render_user_panel(text, width));
        }
        SubagentTranscriptItem::Steer { text, private } => {
            lines.push(markdown::fit_width(
                &format!(
                    "{STEER_DELIVERED}✓ Steer from {} · delivered\x1b[0m",
                    sender(*private)
                ),
                width,
            ));
            lines.extend(render_user_panel(text, width));
        }
        SubagentTranscriptItem::Assistant(text) => {
            if text.trim().is_empty() {
                return lines;
            }
            lines.extend(markdown::render(text.trim(), width));
        }
        SubagentTranscriptItem::Reasoning { .. } if style.hide_reasoning => return lines,
        SubagentTranscriptItem::Reasoning {
            kind,
            text,
            duration,
        } => {
            if text.trim().is_empty() {
                return lines;
            }
            lines.extend(render_thinking_tag(
                *duration,
                false,
                expanded,
                0,
                style.accent,
                width,
            ));
            if expanded {
                lines.push(String::new());
                lines.extend(render_reasoning(*kind, text, width));
            }
        }
        SubagentTranscriptItem::Tool {
            name,
            arguments,
            output,
            is_error,
        } => lines.extend(tool_view::render(
            name, arguments, output, *is_error, false, None, width, expanded,
        )),
    }
    lines.push(String::new());
    lines
}

fn shows_live_thinking(snapshot: &SubagentSnapshot, style: ItemStyle) -> bool {
    !style.hide_reasoning && !snapshot.live_reasoning.trim().is_empty()
}

/// Live content below the cached transcript: the streaming response, a
/// running tool, a progress spinner, waiting steers and queued turns, and
/// how the run ended.
fn render_snapshot_tail(
    snapshot: &SubagentSnapshot,
    style: ItemStyle,
    thinking_expanded: bool,
    spinner_tick: usize,
) -> Vec<String> {
    let width = style.width;
    let mut lines = Vec::new();
    if shows_live_thinking(snapshot, style) {
        let streaming = snapshot.live_reasoning_duration.is_none() && snapshot.status.is_active();
        lines.extend(render_thinking_tag(
            snapshot.live_reasoning_duration,
            streaming,
            thinking_expanded,
            spinner_tick,
            style.accent,
            width,
        ));
        if thinking_expanded {
            lines.push(String::new());
            lines.extend(render_reasoning(
                snapshot
                    .live_reasoning_kind
                    .unwrap_or(crate::provider::ReasoningKind::Summary),
                &snapshot.live_reasoning,
                width,
            ));
        }
        lines.push(String::new());
    }
    if !snapshot.live_assistant.trim().is_empty() {
        lines.extend(markdown::render(snapshot.live_assistant.trim(), width));
        lines.push(String::new());
    }
    if let Some(tool) = &snapshot.current_tool {
        lines.extend(tool_view::render(
            &tool.name,
            &tool.arguments,
            &tool.output,
            tool.is_error,
            true,
            Some(tool.started.elapsed()),
            width,
            style.expanded,
        ));
        lines.push(String::new());
    }
    if let Some(loading) = render_loading_line(snapshot, style, spinner_tick) {
        lines.push(loading);
        lines.push(String::new());
    }
    for (index, steer) in snapshot.pending_steers.iter().enumerate() {
        lines.push(markdown::fit_width(
            &format!(
                "{STEER_PENDING}Steer {} from {} · waiting for a safe boundary\x1b[0m",
                index + 1,
                sender(steer.origin == RunOrigin::PrivateUser)
            ),
            width,
        ));
        lines.extend(render_user_panel(&steer.text, width));
        lines.push(String::new());
    }
    for (index, queued) in snapshot.queued_messages.iter().enumerate() {
        lines.push(markdown::fit_width(
            &format!(
                "{QUEUED}Queued {} from {} · runs after the current turn\x1b[0m",
                index + 1,
                sender(queued.origin == RunOrigin::PrivateUser)
            ),
            width,
        ));
        lines.extend(render_user_panel(&queued.text, width));
        lines.push(String::new());
    }
    lines.extend(render_run_end(snapshot, width));
    lines
}

/// A spinner with the child's current step while nothing else on screen
/// shows progress, mirroring the orchestrator's loading row.
fn render_loading_line(
    snapshot: &SubagentSnapshot,
    style: ItemStyle,
    spinner_tick: usize,
) -> Option<String> {
    let activity = snapshot.current_activity.as_str();
    let label = match snapshot.status {
        SubagentStatus::Queued => {
            return Some(markdown::fit_width(
                "\x1b[2m◌ Queued · starts when a running subagent settles\x1b[0m",
                style.width,
            ));
        }
        SubagentStatus::Starting => "Starting…".to_string(),
        SubagentStatus::Canceling => "Canceling…".to_string(),
        SubagentStatus::Done | SubagentStatus::Failed => return None,
        SubagentStatus::Running => match activity {
            "" | "sending" | "responding" | "reasoning" => "Waiting…".to_string(),
            "compacting" => "Compacting conversation…".to_string(),
            other if other.starts_with("running ") => return None,
            other => {
                let mut characters = other.chars();
                let first = characters.next()?.to_uppercase();
                format!("{first}{}…", characters.as_str())
            }
        },
    };
    let announces_step = snapshot.status != SubagentStatus::Running
        || activity.starts_with("preparing ")
        || activity.starts_with("retrying")
        || activity == "loading skill"
        || activity == "compacting";
    let in_flight = !snapshot.live_assistant.trim().is_empty()
        || snapshot.current_tool.is_some()
        || (!style.hide_reasoning
            && !snapshot.live_reasoning.trim().is_empty()
            && snapshot.live_reasoning_duration.is_none());
    if in_flight && !announces_step {
        return None;
    }
    let frame = SPINNER_FRAMES[spinner_tick % SPINNER_FRAMES.len()];
    let accent = foreground_color(style.accent);
    Some(markdown::fit_width(
        &format!("{accent}{frame}\x1b[0m \x1b[2m{label}\x1b[0m"),
        style.width,
    ))
}

/// Warnings while the child works; once it settles, a marker that makes
/// the end of the run unmistakable, followed by any error.
fn render_run_end(snapshot: &SubagentSnapshot, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    if snapshot.status.is_active() {
        if !snapshot.error.is_empty() {
            lines.push(markdown::fit_width("\x1b[33m⚠ Warning\x1b[0m", width));
            lines.extend(markdown::render(&snapshot.error, width));
            lines.push(String::new());
        }
        return lines;
    }
    let (mark, color, label) = match snapshot.latest_outcome {
        Some(RunOutcome::Completed) => ("✓", "\x1b[32m", "Done"),
        Some(RunOutcome::Interrupted) => ("■", "\x1b[33m", "Interrupted"),
        Some(RunOutcome::Failed) => ("✗", "\x1b[31m", "Failed"),
        None if snapshot.status == SubagentStatus::Failed => ("✗", "\x1b[31m", "Failed"),
        None => return lines,
    };
    let requests = snapshot.requests;
    lines.push(markdown::fit_width(
        &format!(
            "{color}{mark} {label}\x1b[0m \x1b[2m· {} · {requests} request{}\x1b[0m",
            format_duration(snapshot.elapsed(Instant::now())),
            if requests == 1 { "" } else { "s" }
        ),
        width,
    ));
    if !snapshot.error.is_empty() {
        lines.extend(markdown::render(&snapshot.error, width));
    }
    lines.push(String::new());
    lines
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::UiColor;
    use crate::subagent::{QueuedSubagentMessage, SubagentId};

    fn snapshot() -> SubagentSnapshot {
        let mut snapshot = SubagentSnapshot::new(
            SubagentId::new(1),
            "narrow dashboard row".into(),
            "default".into(),
            "inspect the project".into(),
            "test:model".into(),
            100,
        );
        snapshot.status = SubagentStatus::Running;
        snapshot.context_tokens = 50;
        snapshot
            .transcript
            .push(Arc::new(SubagentTranscriptItem::Assistant(
                "live transcript content".into(),
            )));
        snapshot.queued_messages.push(QueuedSubagentMessage {
            text: "queued private message".into(),
            origin: RunOrigin::PrivateUser,
        });
        snapshot
    }

    fn state(view: SubagentView) -> ViewState {
        ViewState {
            subagent_snapshots: vec![snapshot()],
            subagent_view: Some(view),
            ..ViewState::test_default()
        }
    }

    fn item_style(width: usize, hide_reasoning: bool) -> ItemStyle {
        ItemStyle {
            width,
            hide_reasoning,
            expanded: false,
            accent: UiColor::WHITE,
        }
    }

    fn plain(lines: &[String]) -> String {
        lines
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn long_snapshot(items: usize) -> SubagentSnapshot {
        let mut snapshot = snapshot();
        snapshot.queued_messages.clear();
        snapshot.transcript = (1..=items)
            .map(|index| {
                Arc::new(SubagentTranscriptItem::Assistant(format!(
                    "line {index:02}"
                )))
            })
            .collect();
        snapshot
    }

    #[test]
    fn cached_transcript_matches_fresh_render_after_append_eviction_resize_and_hiding() {
        let mut snapshot = long_snapshot(3);
        let mut cache = SnapshotRenderCache::default();
        cache.update(item_style(60, false), &snapshot);
        let retained_line = cache.lines[cache.starts[1]].as_ptr();
        snapshot.transcript.remove(0);
        snapshot
            .transcript
            .push(Arc::new(SubagentTranscriptItem::Reasoning {
                kind: crate::provider::ReasoningKind::Full,
                text: "reasoning".into(),
                duration: None,
            }));
        cache.update(item_style(60, false), &snapshot);
        assert_eq!(
            cache.lines[0].as_ptr(),
            retained_line,
            "surviving entries must reuse rendering"
        );
        for (width, hidden) in [(60, false), (20, false), (20, true), (80, false)] {
            cache.update(item_style(width, hidden), &snapshot);
            let fresh = snapshot
                .transcript
                .iter()
                .flat_map(|item| render_snapshot_item(item, item_style(width, hidden), false))
                .collect::<Vec<_>>();
            assert_eq!(cache.lines, fresh);
        }
        snapshot.transcript.clear();
        cache.update(item_style(80, false), &snapshot);
        assert!(cache.lines.is_empty());
    }

    #[test]
    fn dashboard_does_not_open_without_tracked_subagents() {
        let mut view = state(SubagentView::Dashboard {
            selected_id: None,
            selected_index: 0,
            confirm_cancel: false,
        });
        // The test manager tracks nothing, so refresh clears the seeded
        // snapshot and the command must not open the dashboard.
        open_dashboard(&mut view);
        assert!(view.subagent_view.is_none());
        assert_eq!(
            empty_dashboard_message(true),
            "No tracked subagents. The dashboard opens once the model spawns one."
        );
        assert_eq!(
            empty_dashboard_message(false),
            "Subagents are off. Enable them with /settings subagents on, then ask the model to spawn one."
        );
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
        assert_eq!(dashboard_cursor, HIDDEN_CURSOR);
        assert!(
            dashboard_frame
                .iter()
                .all(|line| markdown::visible_width(line) <= 20)
        );

        let mut takeover = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let (takeover_frame, takeover_cursor) = render(&mut takeover, &Editor::default(), 20, 8);
        assert_eq!(takeover_frame.len(), 8);
        assert_eq!(takeover_cursor, (6, 4));
        assert!(
            takeover_frame
                .iter()
                .all(|line| markdown::visible_width(line) <= 20)
        );
        assert!(
            markdown::strip_ansi(&takeover_frame[0]).contains("Subagent"),
            "even a narrow takeover names itself a subagent: {:?}",
            takeover_frame[0]
        );
    }

    #[test]
    fn takeover_arrow_keys_pin_and_step_the_transcript() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
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
            Some(SubagentView::Takeover { scroll, .. }) if scroll.pin.is_pinned()
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
            Some(SubagentView::Takeover { scroll, .. }) if !scroll.pin.is_pinned()
        ));
    }

    #[test]
    fn takeover_pinned_scrolling_freezes_and_resumes_at_the_bottom() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        view.subagent_snapshots = vec![long_snapshot(30)];
        let editor = Editor::default();
        let (before, _) = render(&mut view, &editor, 80, 12);

        if let Some(SubagentView::Takeover { scroll, .. }) = view.subagent_view.as_mut() {
            scroll.pin.up(2);
        }
        let (pinned, _) = render(&mut view, &editor, 80, 12);
        assert_ne!(before[1..8], pinned[1..8], "pinning must move the window");

        // Streaming output appends below the pinned window.
        if let Some(snapshot) = view.subagent_snapshots.first_mut() {
            snapshot
                .transcript
                .push(Arc::new(SubagentTranscriptItem::Assistant(
                    "newly generated tail".into(),
                )));
        }
        let (frozen, _) = render(&mut view, &editor, 80, 12);
        assert_eq!(
            pinned[1..8],
            frozen[1..8],
            "pinned window must hold still while output streams"
        );

        // Returning to the bottom releases the pin and follows again.
        if let Some(SubagentView::Takeover { scroll, .. }) = view.subagent_view.as_mut() {
            scroll.pin.down(2);
        }
        let (resumed, _) = render(&mut view, &editor, 80, 12);
        assert_ne!(frozen[1..8], resumed[1..8]);
        assert!(resumed.iter().any(|line| line.contains("newly generated")));
        assert!(matches!(
            &view.subagent_view,
            Some(SubagentView::Takeover { scroll, .. }) if !scroll.pin.is_pinned()
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
            snapshot_id,
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

    #[test]
    fn dashboard_is_compact_and_uses_the_same_column_grid_for_headers_and_data() {
        let mut dashboard = state(SubagentView::Dashboard {
            selected_id: Some("sa-1".into()),
            selected_index: 0,
            confirm_cancel: false,
        });
        let (frame, _) = render(&mut dashboard, &Editor::default(), 110, 24);
        let plain = frame
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .collect::<Vec<_>>();
        let header = plain
            .iter()
            .find(|line| line.contains("STATUS") && line.contains("QUEUE"))
            .expect("wide dashboard should show the full table header");
        let data = plain
            .iter()
            .find(|line| line.contains("sa-1"))
            .expect("the tracked subagent should be visible");
        let column = |line: &str, value: &str| {
            let index = line.find(value).expect("fixture value should be visible");
            markdown::visible_width(&line[..index])
        };
        let right_edge =
            |line: &str, value: &str| column(line, value) + markdown::visible_width(value);
        let last_right_edge = |line: &str, value: &str| {
            let index = line.rfind(value).expect("fixture value should be visible");
            markdown::visible_width(&line[..index]) + markdown::visible_width(value)
        };

        assert_eq!(column(header, "STATUS"), column(data, "running"));
        assert_eq!(column(header, "NAME"), column(data, "narrow dashboard row"));
        assert_eq!(column(header, "ID"), column(data, "sa-1"));
        assert_eq!(column(header, "MODEL"), column(data, "test:model"));
        assert_eq!(right_edge(header, "QUEUE"), last_right_edge(data, "1"));

        let occupied = plain
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(occupied.len(), 9);
        assert!(occupied[0] > 0);
        assert!(occupied[8] < frame.len() - 1);
    }

    #[test]
    fn steers_name_their_sender_and_show_when_they_were_delivered() {
        let mut snapshot = snapshot();
        snapshot.queued_messages.clear();
        snapshot
            .transcript
            .push(Arc::new(SubagentTranscriptItem::Steer {
                text: "focus on the parser".into(),
                private: true,
            }));
        snapshot.pending_steers.push(QueuedSubagentMessage {
            text: "also check the lexer".into(),
            origin: RunOrigin::Model,
        });
        let style = item_style(80, false);
        let delivered = plain(&render_snapshot_item(
            snapshot.transcript.last().expect("steer item"),
            style,
            false,
        ));
        assert!(
            delivered.starts_with("✓ Steer from you · delivered"),
            "{delivered}"
        );
        assert!(delivered.contains("focus on the parser"));
        assert!(!delivered.contains("[steer]"));

        let tail = plain(&render_snapshot_tail(&snapshot, style, false, 0));
        assert!(
            tail.contains("Steer 1 from orchestrator · waiting for a safe boundary"),
            "{tail}"
        );
        assert!(!tail.contains("[steering]"));
    }

    #[test]
    fn takeover_header_and_composer_identify_the_subagent() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let (frame, _) = render(&mut view, &Editor::default(), 100, 20);
        let header = markdown::strip_ansi(&frame[0]);
        assert!(header.contains(" Subagent  narrow dashboard row · sa-1 · test:model"));
        assert!(
            header.trim_end().ends_with("running · 0s · 50% ctx"),
            "{header}"
        );
        assert_eq!(markdown::visible_width(&frame[0]), 100);
        let border = frame
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .find(|line| line.starts_with('┌'))
            .expect("composer border");
        assert!(border.contains(" Steer narrow dashboard row "), "{border}");
        let body = plain(&frame);
        assert!(body.contains("From orchestrator") || body.contains("Queued 1 from you"));
        assert!(body.contains("Queued 1 from you · runs after the current turn"));

        view.subagent_snapshots[0].status = SubagentStatus::Done;
        view.subagent_snapshots[0].latest_outcome = Some(RunOutcome::Completed);
        view.subagent_snapshots[0].queued_messages.clear();
        view.subagent_snapshots[0].requests = 3;
        let (settled, _) = render(&mut view, &Editor::default(), 100, 20);
        let body = plain(&settled);
        assert!(body.contains(" Message narrow dashboard row "), "{body}");
        assert!(body.contains("✓ Done · 0s · 3 requests"), "{body}");
        assert!(markdown::strip_ansi(&settled[0]).contains("completed"));
    }

    #[test]
    fn thinking_collapses_to_a_tag_until_ctrl_o_expands_the_takeover() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let mut snapshot = long_snapshot(1);
        snapshot
            .transcript
            .push(Arc::new(SubagentTranscriptItem::Reasoning {
                kind: crate::provider::ReasoningKind::Full,
                text: "secret plan".into(),
                duration: Some(Duration::from_millis(4_200)),
            }));
        view.subagent_snapshots = vec![snapshot.clone()];
        let mut editor = Editor::default();
        let (collapsed, _) = render(&mut view, &editor, 80, 20);
        let body = plain(&collapsed);
        assert!(body.contains("+ Thought: 4.2s"), "{body}");
        assert!(!body.contains("secret plan"));

        handle_event(&mut view, &mut editor, Event::Key(Key::Ctrl('o')));
        view.subagent_snapshots = vec![snapshot];
        let (expanded, _) = render(&mut view, &editor, 80, 20);
        let body = plain(&expanded);
        assert!(body.contains("- Thought: 4.2s"), "{body}");
        assert!(body.contains("secret plan"));
        assert!(body.contains("Ctrl+O collapse"));
    }

    #[test]
    fn live_tail_shows_a_spinner_only_without_other_progress() {
        let mut snapshot = long_snapshot(1);
        snapshot.current_activity = "sending".into();
        let style = item_style(80, false);
        assert!(plain(&render_snapshot_tail(&snapshot, style, false, 0)).contains("Waiting…"));

        snapshot.current_activity = "running shell".into();
        snapshot.current_tool = Some(crate::subagent::LiveTool {
            name: "shell".into(),
            arguments: r#"{"command":"cargo test"}"#.into(),
            output: String::new(),
            is_error: false,
            started: Instant::now(),
        });
        let running = plain(&render_snapshot_tail(&snapshot, style, false, 0));
        assert!(running.contains("$ cargo test  [running"), "{running}");
        assert!(!running.contains("Waiting…"));

        snapshot.current_tool = None;
        snapshot.status = SubagentStatus::Queued;
        let queued = plain(&render_snapshot_tail(&snapshot, style, false, 0));
        assert!(queued.contains("Queued · starts when a running subagent settles"));
    }

    #[test]
    fn tool_output_keeps_its_lines_in_the_takeover() {
        let item = SubagentTranscriptItem::Tool {
            name: "shell".into(),
            arguments: r#"{"command":"ls"}"#.into(),
            output: "first\nsecond".into(),
            is_error: false,
        };
        let lines = render_snapshot_item(&item, item_style(60, false), false)
            .iter()
            .map(|line| markdown::strip_ansi(line).trim().to_string())
            .collect::<Vec<_>>();
        assert!(lines.iter().any(|line| line == "first"), "{lines:#?}");
        assert!(lines.iter().any(|line| line == "second"), "{lines:#?}");
    }

    fn click(view: &mut ViewState, editor: &mut Editor, row: usize) {
        for kind in [MouseKind::Press, MouseKind::Release] {
            handle_event(
                view,
                editor,
                Event::Mouse(crate::tui::events::MouseEvent {
                    kind,
                    column: 4,
                    row,
                }),
            );
        }
    }

    fn row_of(frame: &[String], needle: &str) -> usize {
        frame
            .iter()
            .position(|line| markdown::strip_ansi(line).contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} missing from {:#?}", plain(frame)))
    }

    #[test]
    fn clicking_a_thinking_tag_or_tool_card_toggles_only_that_item() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let mut snapshot = long_snapshot(1);
        for item in [
            SubagentTranscriptItem::Reasoning {
                kind: crate::provider::ReasoningKind::Full,
                text: "first plan".into(),
                duration: Some(Duration::from_secs(1)),
            },
            SubagentTranscriptItem::Tool {
                name: "shell".into(),
                arguments: r#"{"command":"seq 20"}"#.into(),
                output: (1..=20)
                    .map(|line| format!("out {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                is_error: false,
            },
            SubagentTranscriptItem::Reasoning {
                kind: crate::provider::ReasoningKind::Full,
                text: "second plan".into(),
                duration: Some(Duration::from_secs(2)),
            },
        ] {
            snapshot.transcript.push(Arc::new(item));
        }
        let mut editor = Editor::default();
        let render_frame = |view: &mut ViewState, editor: &Editor| {
            view.subagent_snapshots = vec![snapshot.clone()];
            render(view, editor, 80, 60).0
        };

        let frame = render_frame(&mut view, &editor);
        assert!(!plain(&frame).contains("first plan"));
        click(&mut view, &mut editor, row_of(&frame, "+ Thought: 1.0s"));
        let frame = render_frame(&mut view, &editor);
        let body = plain(&frame);
        assert!(body.contains("first plan"), "{body}");
        assert!(
            !body.contains("second plan"),
            "only the clicked block expands"
        );

        // Reasoning text is not a toggle target; only its tag row is.
        click(&mut view, &mut editor, row_of(&frame, "first plan"));
        let frame = render_frame(&mut view, &editor);
        assert!(plain(&frame).contains("first plan"));

        assert!(!plain(&frame).contains("out 20"));
        click(&mut view, &mut editor, row_of(&frame, "out 2"));
        let frame = render_frame(&mut view, &editor);
        assert!(
            plain(&frame).contains("out 20"),
            "tool cards expand from any row"
        );

        click(&mut view, &mut editor, row_of(&frame, "- Thought: 1.0s"));
        let frame = render_frame(&mut view, &editor);
        assert!(
            !plain(&frame).contains("first plan"),
            "a second click collapses"
        );
    }

    #[test]
    fn clicking_the_live_thinking_tag_expands_it_and_keeps_it_open_once_settled() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let mut snapshot = long_snapshot(1);
        snapshot.live_reasoning = "streaming plan".into();
        snapshot.live_reasoning_kind = Some(crate::provider::ReasoningKind::Full);
        snapshot.live_reasoning_started = Some(Instant::now());
        view.subagent_snapshots = vec![snapshot.clone()];
        let mut editor = Editor::default();
        let (frame, _) = render(&mut view, &editor, 80, 30);
        assert!(!plain(&frame).contains("streaming plan"));
        click(&mut view, &mut editor, row_of(&frame, "Thinking"));
        view.subagent_snapshots = vec![snapshot.clone()];
        let (frame, _) = render(&mut view, &editor, 80, 30);
        assert!(
            plain(&frame).contains("streaming plan"),
            "{}",
            plain(&frame)
        );

        snapshot.live_reasoning.clear();
        snapshot.live_reasoning_kind = None;
        snapshot
            .transcript
            .push(Arc::new(SubagentTranscriptItem::Reasoning {
                kind: crate::provider::ReasoningKind::Full,
                text: "streaming plan".into(),
                duration: Some(Duration::from_secs(3)),
            }));
        view.subagent_snapshots = vec![snapshot];
        let (frame, _) = render(&mut view, &editor, 80, 30);
        let body = plain(&frame);
        assert!(body.contains("- Thought: 3.0s"), "{body}");
        assert!(body.contains("streaming plan"));
    }

    #[test]
    fn ticks_animate_the_takeover_and_every_wheel_step_scrolls() {
        let mut view = state(SubagentView::Takeover {
            id: "sa-1".into(),
            scroll: TakeoverScroll::default(),
            confirm_cancel: false,
            expanded: false,
        });
        let mut editor = Editor::default();
        let before = view.spinner_tick;
        handle_event(&mut view, &mut editor, Event::Tick);
        assert_eq!(view.spinner_tick, before + 1);

        view.subagent_snapshots = vec![long_snapshot(30)];
        let (tail, _) = render(&mut view, &editor, 80, 12);
        handle_event(&mut view, &mut editor, Event::MouseScroll(1));
        view.subagent_snapshots = vec![long_snapshot(30)];
        let (stepped, _) = render(&mut view, &editor, 80, 12);
        assert_eq!(
            stepped[2..8],
            tail[1..7],
            "a one-row wheel step (Ctrl+wheel, touch) moves one line"
        );
    }
}
