//! Full-screen frame composition and transcript presentation.

use crate::config::UiColor;
use crate::provider::ReasoningKind;

use super::completion::matching_completions;
use super::input::Editor;
use super::picker::render_picker;
use super::state::{ScrollGeometry, scroll_bar_position, scroll_bar_span};
use super::transcript::Entry;
use super::{USER_BACKGROUND, USER_TEXT, ViewState, markdown, tool_view};

pub(super) fn render_entries(
    entries: &[Entry],
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        match entry {
            Entry::User(content) => lines.extend(render_user_panel(content, width)),
            Entry::Assistant(content) => {
                if content.trim().is_empty() {
                    continue;
                }
                lines.extend(markdown::render(content.trim(), width));
            }
            Entry::Reasoning { kind, content } => {
                if hide_reasoning || content.trim().is_empty() {
                    continue;
                }
                lines.extend(render_reasoning(*kind, content, width));
            }
            Entry::Tool {
                name,
                args,
                output,
                is_error,
                running,
            } => lines.extend(tool_view::render(
                name,
                args,
                output,
                *is_error,
                *running,
                width,
                tools_expanded,
            )),
            Entry::Notice(content) => {
                lines.push("\x1b[1;33mYawl\x1b[0m".into());
                lines.extend(markdown::render(content, width));
            }
        }
        lines.push(String::new());
    }
    lines
}

pub(super) fn render_reasoning(kind: ReasoningKind, content: &str, width: usize) -> Vec<String> {
    const STYLE: &str = "\x1b[2;3;38;2;148;148;158m";
    let continuation = format!("\x1b[0m{STYLE}");
    let style = |line: String| format!("{STYLE}{}\x1b[0m", line.replace("\x1b[0m", &continuation));
    match kind {
        ReasoningKind::Summary => {
            let summary = content.split_whitespace().collect::<Vec<_>>().join(" ");
            vec![style(markdown::fit_width(&summary, width))]
        }
        ReasoningKind::Full => markdown::render(content.trim(), width)
            .into_iter()
            .map(style)
            .collect(),
    }
}

pub(super) fn render_user_panel(content: &str, width: usize) -> Vec<String> {
    let panel_width = width.max(1);
    let horizontal_padding = usize::from(panel_width >= 3);
    let content_width = panel_width.saturating_sub(horizontal_padding * 2).max(1);
    let blank = format!("{USER_BACKGROUND}{}\x1b[0m", " ".repeat(panel_width));
    let continuation = format!("\x1b[0m{USER_BACKGROUND}{USER_TEXT}");
    let mut lines = Vec::new();
    lines.push(blank.clone());
    lines.extend(
        markdown::render(content, content_width)
            .into_iter()
            .map(|line| {
                let fitted =
                    markdown::fit_width(&line, content_width).replace("\x1b[0m", &continuation);
                format!(
                    "{USER_BACKGROUND}{USER_TEXT}{}{fitted}{}\x1b[0m",
                    " ".repeat(horizontal_padding),
                    " ".repeat(horizontal_padding)
                )
            }),
    );
    lines.push(blank);
    lines
}

pub(super) fn render_queued_panel(content: &str, position: usize, width: usize) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;33mQueued {position} · waiting for the active response\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

pub(super) fn foreground_color(color: UiColor) -> String {
    format!("\x1b[38;2;{};{};{}m", color.red, color.green, color.blue)
}

pub(super) fn status_style(color: UiColor) -> String {
    let luminance =
        u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114;
    let text = if luminance >= 150_000 { 24 } else { 245 };
    format!(
        "\x1b[38;2;{text};{text};{text};48;2;{};{};{}m",
        color.red, color.green, color.blue
    )
}

pub(super) fn render_copy_toast(frame: &mut [String], columns: usize, accent: UiColor) {
    const WIDTH: usize = 11;
    let color = foreground_color(accent);
    let toast = [
        format!("{color}┌─────────┐\x1b[0m"),
        format!("{color}│\x1b[0m Copied! {color}│\x1b[0m"),
        format!("{color}└─────────┘\x1b[0m"),
    ];
    let left_width = columns.saturating_sub(WIDTH);
    for (line, toast_line) in frame.iter_mut().zip(toast) {
        *line = format!("{}{toast_line}", markdown::fit_width(line, left_width));
    }
}

/// Track and thumb shading as a fraction of the accent color. The thumb stays
/// brighter than the track so it cannot disappear into a dark background.
const SCROLL_TRACK_INTENSITY: f32 = 0.38;
const SCROLL_THUMB_INTENSITY: f32 = 0.65;

fn shaded_background(color: UiColor, intensity: f32) -> String {
    let channel = |value: u8| (f32::from(value) * intensity).round() as u8;
    format!(
        "\x1b[48;2;{};{};{}m",
        channel(color.red),
        channel(color.green),
        channel(color.blue)
    )
}

pub(super) fn apply_scroll_bar(
    region: &mut [String],
    state: &mut ViewState,
    total_lines: usize,
    max_scroll: usize,
    columns: usize,
) {
    let height = region.len();
    if !state.show_scroll_bar
        || state.picker.is_some()
        || max_scroll == 0
        || height == 0
        || total_lines <= height
        || columns < 2
    {
        state.scroll_geometry = None;
        return;
    }
    let (thumb_length, travel) = scroll_bar_span(height, total_lines);
    state.scroll_geometry = Some(ScrollGeometry {
        rows: height,
        columns,
        max_scroll,
        travel,
        thumb_length,
    });
    let start = scroll_bar_position(travel, max_scroll, state.scroll_offset);
    let track = shaded_background(state.accent_color, SCROLL_TRACK_INTENSITY);
    let thumb = shaded_background(state.accent_color, SCROLL_THUMB_INTENSITY);
    for (row, line) in region.iter_mut().enumerate() {
        let style = if row >= start && row < start + thumb_length {
            &thumb
        } else {
            &track
        };
        debug_assert_eq!(markdown::visible_width(line), columns - 1);
        line.push_str("\x1b[0m");
        line.push_str(style);
        line.push_str(" \x1b[0m");
    }
}

fn render_transcript(state: &ViewState, width: usize) -> Vec<String> {
    let mut transcript = render_entries(
        state.transcript.entries(),
        width,
        state.tools_expanded,
        state.hide_reasoning,
    );
    if let Some(loading) = render_loading_state(state, width) {
        transcript.push(loading);
        transcript.push(String::new());
    }
    for (index, input) in state.queued_inputs.iter().enumerate() {
        transcript.extend(render_queued_panel(input, index + 1, width));
    }
    transcript
}

pub(super) fn has_visible_in_flight_content(state: &ViewState) -> bool {
    let Some(last) = state.transcript.entries().last() else {
        return false;
    };
    match last {
        Entry::User(_) | Entry::Notice(_) => false,
        Entry::Tool { running, .. } => *running,
        Entry::Reasoning { content, .. } => !state.hide_reasoning && !content.trim().is_empty(),
        Entry::Assistant(content) => !content.trim().is_empty(),
    }
}

pub(super) fn loading_label(activity: &str) -> Option<&str> {
    match activity {
        "sending" | "responding" | "reasoning" => Some("Waiting…"),
        "compacting conversation" => Some("Compacting conversation…"),
        "canceling turn" => Some("Canceling turn…"),
        other if other.starts_with("attempt") => Some(other),
        _ => None,
    }
}

pub(super) fn render_loading_state(state: &ViewState, width: usize) -> Option<String> {
    let label = loading_label(&state.activity)?;
    if has_visible_in_flight_content(state) {
        return None;
    }
    const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
    let accent = foreground_color(state.accent_color);
    let rendered = format!(" {accent}{frame}\x1b[0m \x1b[2m{label}\x1b[0m");
    Some(markdown::fit_width(&rendered, width))
}

pub(super) fn build_frame(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(20);
    let rows = rows.max(8);
    let inner_width = columns.saturating_sub(2);
    let layout = editor.layout(inner_width);
    let max_input_lines = (rows / 3).max(1);
    let input_start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(max_input_lines)
        .min(layout.lines.len().saturating_sub(max_input_lines));
    let input_end = (input_start + max_input_lines).min(layout.lines.len());
    let input_lines = &layout.lines[input_start..input_end];
    let cursor_input_row = layout.cursor_row.saturating_sub(input_start);
    let input_height = input_lines.len() + 2;
    let menu_capacity = rows.saturating_sub(input_height + 1);
    let completions = if state.picker.is_none() {
        matching_completions(&state.completions, editor)
    } else {
        Vec::new()
    };
    let match_count = completions.len().min(menu_capacity);
    if match_count > 0 {
        state.completion_index = state.completion_index.min(match_count - 1);
    }
    let menu = if state.picker.is_none() {
        completions
            .into_iter()
            .take(menu_capacity)
            .enumerate()
            .map(|(index, completion)| {
                let line = format!("  {:<18} {}", completion.command, completion.description);
                if index == state.completion_index {
                    format!("\x1b[7m{}\x1b[0m", markdown::fit_width(&line, columns))
                } else {
                    markdown::fit_width(&line, columns)
                }
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let menu_height = menu.len();
    let transcript_height = rows.saturating_sub(input_height + menu_height + 1);
    let mut transcript = render_transcript(state, columns);
    let show_scroll_bar = state.show_scroll_bar
        && state.picker.is_none()
        && columns >= 2
        && transcript.len() > transcript_height;
    let transcript_width = if show_scroll_bar {
        columns - 1
    } else {
        columns
    };
    if show_scroll_bar {
        transcript = render_transcript(state, transcript_width);
    }
    let max_scroll = transcript.len().saturating_sub(transcript_height);
    state.scroll_offset = state.scroll_offset.min(max_scroll);
    let end = transcript.len().saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(transcript_height);
    let visible = &transcript[start..end];

    let mut region = Vec::with_capacity(transcript_height);
    if let Some(picker) = &state.picker {
        region.extend(render_picker(picker, editor, columns, transcript_height));
    } else {
        region.extend(std::iter::repeat_n(
            " ".repeat(transcript_width),
            transcript_height.saturating_sub(visible.len()),
        ));
        region.extend(
            visible
                .iter()
                .map(|line| markdown::fit_width(line, transcript_width)),
        );
    }
    apply_scroll_bar(&mut region, state, transcript.len(), max_scroll, columns);
    let mut frame = region;
    frame.extend(menu);
    let text_box_color = foreground_color(state.accent_color);
    frame.push(format!(
        "{text_box_color}┌{}┐\x1b[0m",
        "─".repeat(inner_width)
    ));
    for line in input_lines {
        frame.push(format!(
            "{text_box_color}│\x1b[0m{}{text_box_color}│\x1b[0m",
            markdown::fit_width(line, inner_width)
        ));
    }
    frame.push(format!(
        "{text_box_color}└{}┘\x1b[0m",
        "─".repeat(inner_width)
    ));

    let percentage = state
        .context_tokens
        .saturating_mul(100)
        .checked_div(state.context_window)
        .unwrap_or(0);
    let reasoning = state
        .reasoning_effort
        .as_deref()
        .map_or(String::new(), |effort| format!(" · {effort}"));
    let mut status = format!(
        " {}{}  {}/{} tokens ({}%)",
        state.model, reasoning, state.context_tokens, state.context_window, percentage
    );
    if !state.activity.is_empty() {
        status.push_str("  ");
        status.push_str(&state.activity);
    }
    if !state.queued_inputs.is_empty() {
        status.push_str(&format!("  {} queued", state.queued_inputs.len()));
    }
    if !state.pending_actions.is_empty() {
        status.push_str(&format!("  {} change pending", state.pending_actions.len()));
    }
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(state.accent_color),
        markdown::fit_width(&status, columns)
    ));

    if state.copy_toast_ticks > 0 {
        render_copy_toast(&mut frame, columns, state.accent_color);
    }

    let cursor_row = transcript_height + menu_height + 2 + cursor_input_row;
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}
