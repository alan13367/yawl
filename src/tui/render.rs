//! Full-screen frame composition and transcript presentation.
//!
//! This facade owns frame composition. Private children own the transcript
//! cache with entry rendering and the welcome animation. TUI callers keep
//! the entry points here.

mod cache;
mod welcome;

pub(super) use cache::{
    RenderCache, loading_label, render_reasoning, render_transcript_window, render_user_panel,
};
use cache::{label_refs, render_entry, subagent_labels};
#[cfg(test)]
pub(super) use cache::{render_entries, render_loading_state, render_queued_panel};
pub(super) use welcome::WELCOME_ANIMATION_TICKS;
use welcome::render_welcome;

use std::sync::Arc;

use crate::config::UiColor;
use crate::provider::ImageContent;

use super::completion::{
    COMPLETION_MENU_ROWS, completion_window, menu_rows, sync_completion_filter,
};
use super::input::Editor;
use super::picker::{Picker, PickerAction, picker_is_plan_handoff, render_picker};
use super::state::{ScrollGeometry, scroll_bar_position, scroll_bar_span};
use super::{ViewState, markdown};

const BACKGROUND_NOTICE_CYAN: UiColor = UiColor::new(116, 199, 213);
const BACKGROUND_NOTICE_AMBER: UiColor = UiColor::new(232, 202, 118);

/// Invalid one-based terminal coordinates signal views that have no editor
/// and must leave the hardware cursor hidden.
pub(super) const HIDDEN_CURSOR: (usize, usize) = (0, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageSupport {
    None,
    Png,
    All,
}

impl ImageSupport {
    fn accepts(self, media_type: &str) -> bool {
        match self {
            Self::None => false,
            Self::Png => media_type == "image/png",
            Self::All => matches!(
                media_type,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ),
        }
    }
}

#[derive(Clone)]
pub(super) struct FrameImage {
    pub(super) key: usize,
    pub(super) row: usize,
    pub(super) column: usize,
    pub(super) columns: usize,
    pub(super) rows: usize,
    pub(super) content: Arc<ImageContent>,
}

pub(super) struct RenderedFrame {
    pub(super) lines: Vec<String>,
    pub(super) cursor: (usize, usize),
    pub(super) images: Vec<FrameImage>,
}

pub(super) fn foreground_color(color: UiColor) -> String {
    format!("\x1b[38;2;{};{};{}m", color.red, color.green, color.blue)
}

/// Selection highlight for menu, picker, and dashboard rows: the configured
/// selection color as the background with near-black or near-white text
/// chosen by luminance. The 120,000 threshold (of a 255,000 maximum) is
/// where both text choices contrast about equally, so every color keeps the
/// selected row readable.
pub(super) fn selection_style(color: UiColor) -> String {
    let luminance =
        u32::from(color.red) * 299 + u32::from(color.green) * 587 + u32::from(color.blue) * 114;
    let text = if luminance >= 120_000 { 16 } else { 250 };
    format!(
        "\x1b[38;2;{text};{text};{text};48;2;{};{};{}m",
        color.red, color.green, color.blue
    )
}

/// Paints one already-fitted row with the selection style, re-arming the
/// highlight after embedded resets so colored fragments (swatches, status
/// squares) cannot cut the bar short.
pub(super) fn selected_row(line: &str, style: &str) -> String {
    let continuation = format!("\x1b[0m{style}");
    format!("{style}{}\x1b[0m", line.replace("\x1b[0m", &continuation))
}

/// Muted accent foreground for status and hint lines: the accent blended
/// toward light gray so dark accents stay legible without a background.
pub(super) fn status_style(color: UiColor) -> String {
    let channel = |value: u8| ((u16::from(value) + 200) / 2) as u8;
    format!(
        "\x1b[38;2;{};{};{}m",
        channel(color.red),
        channel(color.green),
        channel(color.blue)
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

/// Thumb shading as a fraction of the accent color.
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
        || state
            .picker
            .as_ref()
            .is_some_and(|picker| !picker_is_plan_handoff(picker))
        || max_scroll == 0
        || height == 0
        || total_lines <= height
        || columns == 0
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
    let thumb = shaded_background(state.accent_color, SCROLL_THUMB_INTENSITY);
    for (row, line) in region.iter_mut().enumerate() {
        debug_assert_eq!(markdown::visible_width(line), columns);
        if row >= start && row < start + thumb_length {
            *line = markdown::overlay_last_cell_background(line, &thumb);
        }
    }
}

pub(super) fn build_frame(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let rendered = build_frame_with_images(state, editor, columns, rows, ImageSupport::None);
    (rendered.lines, rendered.cursor)
}

pub(super) fn build_frame_with_images(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
    image_support: ImageSupport,
) -> RenderedFrame {
    if state.git_init.is_some() {
        let (lines, cursor) = super::git::render_init(state, editor, columns, rows);
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    if state.process_view.is_some() {
        let (lines, cursor) = super::processes::render(state, columns, rows);
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    if state.git_view.is_some() {
        let (lines, cursor) = super::git::render(state, editor, columns, rows);
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    if state.subagent_view.is_some() {
        let (lines, cursor) = super::subagents::render(state, editor, columns, rows);
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    let columns = columns.max(20);
    let rows = rows.max(8);
    if state.transcript.viewer_open() {
        let (lines, cursor) = render_block_viewer(state, columns, rows);
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    let inner_width = columns.saturating_sub(2);
    let plan_handoff = state.picker.as_ref().is_some_and(picker_is_plan_handoff);
    let layout = if super::picker::picker_is_secret(state) {
        editor.masked_layout(inner_width)
    } else {
        editor.layout(inner_width)
    };
    let background_count = state.background_processes.active_count();
    state.background_active_count = background_count;
    let background_notice_height = usize::from(background_count > 0);
    let status_line = super::status_bar::render(state, columns);
    let status_height = usize::from(status_line.is_some());
    let search_height = usize::from(state.transcript.search_active());
    let max_input_lines = (rows / 3).max(1);
    let max_question_lines = rows
        .saturating_sub(2 + background_notice_height + status_height + search_height)
        .max(1);
    let (input_lines, cursor_input_row, cursor_input_col, hide_input_cursor) =
        if let Some(question) = &state.question {
            let (lines, cursor, focus_row) =
                render_question_input(question, inner_width, state.selection_color, rows <= 10);
            let (lines, cursor) = bounded_input_view(lines, cursor, focus_row, max_question_lines);
            let (cursor_row, cursor_col) = cursor.unwrap_or((0, 0));
            (lines, cursor_row, cursor_col, cursor.is_none())
        } else if let Some(picker) = state
            .picker
            .as_ref()
            .filter(|picker| picker_is_plan_handoff(picker))
        {
            let (lines, focus_row) =
                render_plan_handoff_input(picker, inner_width, state.selection_color, rows <= 10);
            let (lines, _) = bounded_input_view(lines, None, focus_row, max_input_lines);
            (lines, 0, 0, true)
        } else {
            let input_start = layout
                .cursor_row
                .saturating_add(1)
                .saturating_sub(max_input_lines)
                .min(layout.lines.len().saturating_sub(max_input_lines));
            let input_end = (input_start + max_input_lines).min(layout.lines.len());
            (
                layout.lines[input_start..input_end].to_vec(),
                layout.cursor_row.saturating_sub(input_start),
                layout.cursor_col,
                false,
            )
        };
    let input_height = input_lines.len() + 2;
    let menu_capacity = COMPLETION_MENU_ROWS
        .min(rows.saturating_sub(input_height + background_notice_height + status_height));
    let menu_entries = if state.picker.is_none() && state.question.is_none() {
        sync_completion_filter(state, editor);
        menu_rows(state, editor)
    } else {
        Vec::new()
    };
    if !menu_entries.is_empty() {
        state.completion_index = state.completion_index.min(menu_entries.len() - 1);
    }
    let window = completion_window(menu_entries.len(), state.completion_index, menu_capacity);
    let mut menu = menu_entries[window.clone()]
        .iter()
        .zip(window.clone())
        .map(|((label, detail), index)| {
            let line = format!("  {label:<18} {detail}");
            if index == state.completion_index {
                selected_row(
                    &markdown::fit_width(&line, columns),
                    &selection_style(state.selection_color),
                )
            } else {
                markdown::fit_width(&line, columns)
            }
        })
        .collect::<Vec<_>>();
    if menu_entries.len() > window.len() {
        let accent = foreground_color(state.accent_color);
        let above = window.start;
        let below = menu_entries.len() - window.end;
        let describe = |count: usize, wraps_to: &str| {
            if count > 0 {
                format!("{count} more")
            } else {
                format!("wraps to {wraps_to}")
            }
        };
        menu.insert(
            0,
            markdown::fit_width(
                &format!(
                    "  {accent}↑\x1b[0m \x1b[2m{}\x1b[0m",
                    describe(above, "end")
                ),
                columns,
            ),
        );
        menu.push(markdown::fit_width(
            &format!(
                "  {accent}↓\x1b[0m \x1b[2m{} · {}/{}\x1b[0m",
                describe(below, "start"),
                state.completion_index + 1,
                menu_entries.len()
            ),
            columns,
        ));
    }
    let menu_height = menu.len();
    let transcript_height = rows.saturating_sub(
        input_height + menu_height + search_height + background_notice_height + status_height,
    );
    let transcript = render_transcript_window(state, columns, transcript_height, image_support);
    let transcript_width = columns;
    let visible = &transcript.lines;

    let mut region = Vec::with_capacity(transcript_height);
    if let Some(picker) = state
        .picker
        .as_ref()
        .filter(|picker| !picker_is_plan_handoff(picker))
    {
        let outline = foreground_color(state.accent_color);
        region.extend(render_picker(
            picker,
            editor,
            &selection_style(state.selection_color),
            &outline,
            columns,
            transcript_height,
        ));
    } else if visible.is_empty() && state.transcript.is_empty() {
        region.extend(render_welcome(
            state.accent_color,
            transcript_width,
            transcript_height,
            state.spinner_tick,
        ));
    } else {
        region.extend(std::iter::repeat_n(
            " ".repeat(transcript_width),
            transcript_height.saturating_sub(visible.len()),
        ));
        let selected = state.transcript.selected_index();
        let focused = state.transcript.is_focused();
        region.extend(visible.iter().map(|(line, owner)| {
            let line = markdown::fit_width(line, transcript_width);
            if focused && *owner == selected && transcript_width > 1 {
                format!(
                    "{}▌\x1b[0m{}",
                    foreground_color(state.accent_color),
                    markdown::fit_width(&line, transcript_width - 1)
                )
            } else {
                line
            }
        }));
    }
    apply_scroll_bar(
        &mut region,
        state,
        transcript.total_lines,
        transcript.max_scroll,
        columns,
    );
    let mut frame = region;
    if let Some(query) = state.transcript.search_query() {
        let position = state.transcript.search_position().map_or_else(
            || "0/0".to_string(),
            |(current, total)| format!("{current}/{total}"),
        );
        frame.push(markdown::fit_width(
            &format!(
                " {}Find\x1b[0m  {query}  \x1b[2m{position} · Enter next · ↑ previous · Esc close\x1b[0m",
                foreground_color(state.accent_color)
            ),
            columns,
        ));
    }
    let text_box_color = foreground_color(state.accent_color);
    let composer_label = if plan_handoff {
        Some("Plan ready")
    } else if state.turn_started.is_some() && !state.goal_running && state.plan_draft {
        Some("Planning")
    } else if state.turn_started.is_some() && !state.goal_running && state.active_plan.is_some() {
        Some("Plan turn")
    } else {
        None
    };
    let top_border = composer_top_border(inner_width, composer_label);
    frame.push(format!("{text_box_color}┌{}┐\x1b[0m", top_border));
    for line in &input_lines {
        frame.push(format!(
            "{text_box_color}│\x1b[0m{}{text_box_color}│\x1b[0m",
            markdown::fit_width(line, inner_width)
        ));
    }
    let show_queue_hint = state.question.is_none()
        && state.turn_started.is_some()
        && !editor.is_empty()
        && menu.is_empty();
    let bottom_border = if show_queue_hint {
        const HINT: &str = " Tab queues ";
        format!(
            "{}{}",
            "─".repeat(inner_width.saturating_sub(HINT.len())),
            HINT
        )
    } else {
        "─".repeat(inner_width)
    };
    frame.push(format!("{text_box_color}└{bottom_border}┘\x1b[0m"));
    frame.extend(menu);

    if background_count > 0 {
        frame.push(render_background_process_notice(
            background_count,
            columns,
            state.accent_color,
        ));
    }

    if let Some(status_line) = status_line {
        frame.push(status_line);
    }

    if state.copy_toast_ticks > 0 {
        render_copy_toast(&mut frame, columns, state.accent_color);
    }

    let (cursor_row, cursor_col) = if hide_input_cursor || plan_handoff {
        HIDDEN_CURSOR
    } else if let Some(query) = state.transcript.search_query() {
        (
            transcript_height + 1,
            (8 + markdown::visible_width(query)).min(columns.saturating_sub(1)),
        )
    } else {
        (
            transcript_height + search_height + 2 + cursor_input_row,
            (2 + cursor_input_col).min(columns.saturating_sub(1)),
        )
    };
    let images = if state
        .picker
        .as_ref()
        .is_some_and(|picker| !picker_is_plan_handoff(picker))
    {
        Vec::new()
    } else {
        transcript.images
    };
    RenderedFrame {
        lines: frame,
        cursor: (cursor_row, cursor_col),
        images,
    }
}

fn composer_top_border(width: usize, label: Option<&str>) -> String {
    let Some(label) = label else {
        return "─".repeat(width);
    };
    let label = format!(" {label} ");
    if label.len().saturating_add(1) > width {
        return "─".repeat(width);
    }
    format!(
        "─{label}{}",
        "─".repeat(width.saturating_sub(label.len() + 1))
    )
}

fn render_plan_handoff_input(
    picker: &Picker,
    width: usize,
    selection_color: UiColor,
    compact: bool,
) -> (Vec<String>, usize) {
    let mut lines = Vec::with_capacity(picker.items.len() + 2);
    let mut focus_row = 0;
    if !compact {
        lines.push(markdown::fit_width("Choose what happens next", width));
    }
    for (index, item) in picker.items.iter().enumerate() {
        if index == picker.selected {
            focus_row = lines.len();
        }
        lines.extend(render_choice_lines(
            index,
            &item.label,
            &item.description,
            matches!(item.action, PickerAction::ImplementPlan),
            index == picker.selected,
            width,
            selection_color,
        ));
    }
    if !compact {
        lines.push(markdown::fit_width(
            " ↑/↓ choose · 1–2 select · Enter confirm · Esc return",
            width,
        ));
    }
    (lines, focus_row)
}

fn render_question_input(
    active: &super::state::ActiveQuestion,
    width: usize,
    selection_color: UiColor,
    compact: bool,
) -> (Vec<String>, Option<(usize, usize)>, usize) {
    let snapshot = &active.snapshot;
    let countdown = snapshot
        .remaining
        .map(|remaining| format!(" · {}s", remaining.as_secs().saturating_add(1)))
        .unwrap_or_default();
    let heading = if compact {
        format!(
            "{}/{}{countdown} · {}",
            snapshot.question_index + 1,
            snapshot.question_count,
            snapshot.question.question
        )
    } else {
        format!(
            "Question {}/{}{countdown}",
            snapshot.question_index + 1,
            snapshot.question_count
        )
    };
    let mut lines = markdown::wrapped_plain_lines(&heading, width);
    if !compact {
        lines.extend(markdown::wrapped_plain_lines(
            &snapshot.question.question,
            width,
        ));
    }

    if let super::state::QuestionInput::Custom(editor) = &active.input {
        if !compact {
            lines.push(String::new());
            lines.push(markdown::fit_width(" Your answer", width));
        }
        let layout = editor.layout(width);
        let answer_start = lines.len();
        let cursor = (answer_start + layout.cursor_row, layout.cursor_col);
        lines.extend(
            layout
                .lines
                .into_iter()
                .map(|line| markdown::fit_width(&line, width)),
        );
        if !compact {
            lines.push(markdown::fit_width(
                " Enter submit · Shift+Enter newline · Esc choices · Ctrl+C cancel",
                width,
            ));
            lines.push(String::new());
        }
        return (lines, Some(cursor), cursor.0);
    }

    let mut focus_row = lines.len();
    for (index, option) in snapshot.question.options.iter().enumerate() {
        if index == active.selected {
            focus_row = lines.len();
        }
        lines.extend(render_choice_lines(
            index,
            &option.label,
            if compact { "" } else { &option.description },
            index == snapshot.question.recommended,
            index == active.selected,
            width,
            selection_color,
        ));
    }
    let other_index = snapshot.question.options.len();
    if other_index == active.selected {
        focus_row = lines.len();
    }
    lines.extend(render_choice_lines(
        other_index,
        "Other…",
        if compact { "" } else { "Write your own answer" },
        false,
        other_index == active.selected,
        width,
        selection_color,
    ));
    if !compact {
        let option_count = snapshot.question.options.len() + 1;
        lines.push(markdown::fit_width(
            &format!(" ↑/↓ choose · 1–{option_count} select · Enter confirm · Esc cancel"),
            width,
        ));
        lines.push(String::new());
    }
    (lines, None, focus_row)
}

fn bounded_input_view(
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
    focus_row: usize,
    max_lines: usize,
) -> (Vec<String>, Option<(usize, usize)>) {
    let max_lines = max_lines.max(1);
    if lines.len() <= max_lines {
        return (lines, cursor);
    }
    let focus_row = cursor
        .map_or(focus_row, |(row, _)| row)
        .min(lines.len().saturating_sub(1));
    let start = focus_row
        .saturating_sub(max_lines / 2)
        .min(lines.len() - max_lines);
    let end = start + max_lines;
    let cursor = cursor
        .and_then(|(row, column)| (start..end).contains(&row).then_some((row - start, column)));
    (lines[start..end].to_vec(), cursor)
}

fn render_choice_lines(
    index: usize,
    label: &str,
    description: &str,
    recommended: bool,
    selected: bool,
    width: usize,
    selection_color: UiColor,
) -> Vec<String> {
    let prefix = format!(" {}. ", index + 1);
    let continuation = " ".repeat(markdown::visible_width(&prefix));
    let recommended = if recommended { " (Recommended)" } else { "" };
    let content = if description.is_empty() {
        format!("{label}{recommended}")
    } else {
        format!("{label}{recommended} · {description}")
    };
    let style = selection_style(selection_color);
    markdown::wrapped_plain_prefixed_lines(&prefix, &continuation, &content, width)
        .into_iter()
        .map(|line| {
            let line = markdown::fit_width(&line, width);
            if selected {
                selected_row(&line, &style)
            } else {
                line
            }
        })
        .collect()
}

pub(super) fn render_background_process_notice(
    count: usize,
    width: usize,
    accent: UiColor,
) -> String {
    let color = background_notice_color(accent);
    if width < 48 {
        return markdown::fit_width(
            &format!(
                " {}\x1b[1m{count} bg\x1b[22m {}running \u{b7} /ps\x1b[0m",
                foreground_color(color),
                status_style(color),
            ),
            width,
        );
    }
    let noun = if count == 1 {
        "background terminal"
    } else {
        "background terminals"
    };
    markdown::fit_width(
        &format!(
            " {}\u{25cf}  \x1b[1m{count} {noun}\x1b[22m {}running  \u{b7}  /ps to view\x1b[0m",
            foreground_color(color),
            status_style(color),
        ),
        width,
    )
}

fn background_notice_color(accent: UiColor) -> UiColor {
    if perceptual_color_distance(accent, BACKGROUND_NOTICE_CYAN)
        >= perceptual_color_distance(accent, BACKGROUND_NOTICE_AMBER)
    {
        BACKGROUND_NOTICE_CYAN
    } else {
        BACKGROUND_NOTICE_AMBER
    }
}

/// Weighted RGB distance keeps the notice visually separate from both named
/// and custom accent colors without adding another configurable setting.
fn perceptual_color_distance(left: UiColor, right: UiColor) -> u32 {
    let red_mean = (u32::from(left.red) + u32::from(right.red)) / 2;
    let squared_delta =
        |left: u8, right: u8| (i32::from(left) - i32::from(right)).unsigned_abs().pow(2);
    let red = squared_delta(left.red, right.red);
    let green = squared_delta(left.green, right.green);
    let blue = squared_delta(left.blue, right.blue);
    (((512 + red_mean) * red) >> 8) + 4 * green + (((767 - red_mean) * blue) >> 8)
}

fn render_block_viewer(
    state: &mut ViewState,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let Some(index) = state.transcript.selected_index() else {
        state.transcript.close_viewer();
        return build_frame(state, &Editor::default(), columns, rows);
    };
    let labels = subagent_labels(state);
    let label_refs = label_refs(&labels);
    let Some(entry) = state.transcript.entry(index) else {
        state.transcript.close_viewer();
        return build_frame(state, &Editor::default(), columns, rows);
    };
    let label = crate::subagent::sanitize_preview(entry.label(), 128);
    let title = format!(
        " {}{} {} · Esc close · y copy\x1b[0m",
        foreground_color(state.accent_color),
        label,
        index + 1,
    );
    let body = render_entry(
        entry,
        columns,
        true,
        state.hide_reasoning,
        ImageSupport::None,
        state.accent_color,
        &label_refs,
    )
    .unwrap_or_default();
    let body = body.lines;
    let body_height = rows.saturating_sub(2);
    let total = body.len();
    let max_scroll = total.saturating_sub(body_height);
    state.scroll_offset = state.scroll_offset.min(max_scroll);
    let end = total.saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(body_height);
    let visible = &body[start..end];
    let mut frame = Vec::with_capacity(rows);
    frame.push(markdown::fit_width(&title, columns));
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        body_height.saturating_sub(visible.len()),
    ));
    frame.extend(
        visible
            .iter()
            .map(|line| markdown::fit_width(line, columns)),
    );
    frame.push(markdown::fit_width(
        &format!(" \x1b[2m{end}/{total} lines\x1b[0m"),
        columns,
    ));
    (frame, (rows, 1))
}

pub(super) use super::status_bar::format_token_count;
