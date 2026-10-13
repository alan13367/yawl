//! Full-screen frame composition and transcript presentation.
//!
//! This facade owns frame composition. Private children separate cache reuse,
//! entry presentation, reasoning blocks, transcript windows, composer chrome,
//! the scroll bar, and overlays. TUI callers keep the entry points here.

mod activity;
mod cache;
mod composer;
mod entries;
mod questions;
mod reasoning;
mod scroll;
mod transcript;
mod welcome;

pub(super) use activity::activity_color;
use activity::render_activity_rows;
pub(super) use cache::{LineOwner, RenderCache};
pub(super) use composer::composer_top_border;
use composer::{
    bounded_input_view, busy_composer_hint, render_choice_lines, render_plan_handoff_input,
};
use entries::render_entry;
pub(super) use entries::{entry_default_expanded, render_user_panel};
#[cfg(test)]
pub(super) use entries::{render_entries, render_expanded, render_queued_panel};
pub(super) use reasoning::{render_reasoning, render_thinking_tag};
use scroll::{apply_scroll_bar, render_jump_button};
use transcript::subagent_labels;
#[cfg(test)]
pub(super) use transcript::{MODEL_STREAM_STALL, render_loading_state};
pub(super) use transcript::{loading_label, render_transcript_window};
pub(super) use welcome::WELCOME_ANIMATION_TICKS;
pub(super) use welcome::WelcomeDetails;
use welcome::render_welcome;

pub(super) use super::status_bar::format_token_count;

use std::sync::Arc;

use crate::config::UiColor;
use crate::provider::ImageContent;

use super::completion::{
    COMPLETION_MENU_ROWS, completion_window, menu_rows, sync_completion_filter,
};
use super::input::Editor;
use super::picker::{picker_is_plan_handoff, render_picker};
use super::{ViewState, markdown, theme};

pub(super) const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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

/// Foreground escape for an interface color, darkened on light
/// backgrounds when it would be too pale to read.
pub(super) fn foreground_color(color: UiColor) -> String {
    let color = theme::ink(color);
    format!("\x1b[38;2;{};{};{}m", color.red, color.green, color.blue)
}

/// Selection highlight for menu, picker, and dashboard rows: the configured
/// selection color as the background with near-black or near-white text
/// chosen by luminance. The 120,000 threshold (of a 255,000 maximum) is
/// where both text choices contrast about equally, so every color keeps the
/// selected row readable.
pub(super) fn selection_style(color: UiColor) -> String {
    // A pale highlight would vanish on a light background.
    let color = theme::ink(color);
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
    foreground_color(UiColor::new(
        channel(color.red),
        channel(color.green),
        channel(color.blue),
    ))
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

fn frame_without_jump_button(
    state: &mut ViewState,
    lines: Vec<String>,
    cursor: (usize, usize),
) -> RenderedFrame {
    state.follow.button = None;
    state.follow.pointer = None;
    state.transcript_row_entries.clear();
    RenderedFrame {
        lines,
        cursor,
        images: Vec::new(),
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
    // Only the transcript view draws the jump button; every other view must
    // not leave a stale hit area or pointer cell behind. Motion tracking is
    // off on those screens, so a saved cell would highlight the wrong place
    // when the transcript returns.
    state.follow.button = None;
    if state.git_init.is_some() {
        let (lines, cursor) = super::git::render_init(state, editor, columns, rows);
        return frame_without_jump_button(state, lines, cursor);
    }
    if state.process_view.is_some() {
        let (lines, cursor) = super::processes::render(state, columns, rows);
        return frame_without_jump_button(state, lines, cursor);
    }
    if state.git_view.is_some() {
        let (lines, cursor) = super::git::render(state, editor, columns, rows);
        return frame_without_jump_button(state, lines, cursor);
    }
    if state.subagent_view.is_some() {
        let (lines, cursor) = super::subagents::render(state, editor, columns, rows);
        return frame_without_jump_button(state, lines, cursor);
    }
    let columns = columns.max(20);
    let rows = rows.max(8);
    if state.transcript.viewer_open() {
        let (lines, cursor) = render_block_viewer(state, columns, rows);
        // A viewer that closed itself falls through to a transcript frame and
        // owns the jump button that frame just recorded.
        if state.transcript.viewer_open() {
            return frame_without_jump_button(state, lines, cursor);
        }
        state.transcript_row_entries.clear();
        return RenderedFrame {
            lines,
            cursor,
            images: Vec::new(),
        };
    }
    let inner_width = columns.saturating_sub(2);
    let plan_handoff = state.picker.as_ref().is_some_and(picker_is_plan_handoff);
    let layout = editor.styled_layout(
        inner_width,
        &foreground_color(reasoning::thinking_color(state.accent_color)),
    );
    let background_count = state.background_processes.active_count();
    state.background_active_count = background_count;
    let background_snapshots = if background_count > 0 {
        state.background_processes.snapshots()
    } else {
        Vec::new()
    };
    let activity_rows = render_activity_rows(
        &state.subagent_snapshots,
        &background_snapshots,
        columns,
        state.accent_color,
    );
    let activity_height = activity_rows.len();
    let status_line = super::status_bar::render(state, columns);
    let status_height = usize::from(status_line.is_some());
    let search_height = usize::from(state.transcript.search_active());
    let max_input_lines = (rows / 3).max(1);
    let max_question_lines = rows
        .saturating_sub(2 + activity_height + status_height + search_height)
        .max(1);
    let (input_lines, cursor_input_row, cursor_input_col, hide_input_cursor) =
        if let Some(question) = &state.question {
            let (lines, cursor, focus_row) =
                questions::render_input(question, inner_width, state.selection_color, rows <= 10);
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
        .min(rows.saturating_sub(input_height + activity_height + status_height));
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
        input_height + menu_height + search_height + activity_height + status_height,
    );
    let transcript = render_transcript_window(state, columns, transcript_height, image_support);
    let transcript_width = columns;
    let visible = &transcript.lines;

    let mut region = Vec::with_capacity(transcript_height);
    // Click-to-expand hit-testing mirrors exactly what is drawn: padding rows
    // map to `None`, transcript rows map to their owning entry, and picker or
    // welcome screens clear the map because no tool card is visible.
    let mut click_map = Vec::with_capacity(transcript_height);
    let mut showing_transcript = false;
    let mut picker_cursor = None;
    if let Some(picker) = state
        .picker
        .as_ref()
        .filter(|picker| !picker_is_plan_handoff(picker))
    {
        let outline = foreground_color(state.accent_color);
        let (lines, cursor) = render_picker(
            picker,
            editor,
            &selection_style(state.selection_color),
            &outline,
            columns,
            transcript_height,
        );
        region.extend(lines);
        picker_cursor = cursor;
        state.transcript_row_entries.clear();
    } else if visible.is_empty() && state.transcript.is_empty() {
        region.extend(render_welcome(
            state.accent_color,
            &state.welcome,
            transcript_width,
            transcript_height,
            state.spinner_tick,
        ));
        state.transcript_row_entries.clear();
    } else {
        showing_transcript = true;
        click_map.extend(std::iter::repeat_n(
            None,
            transcript_height.saturating_sub(visible.len()),
        ));
        click_map.extend(visible.iter().map(|(_, owner)| *owner));
        state.transcript_row_entries = click_map;
        region.extend(std::iter::repeat_n(
            " ".repeat(transcript_width),
            transcript_height.saturating_sub(visible.len()),
        ));
        let selected = state.transcript.selected_index();
        let focused = state.transcript.is_focused();
        region.extend(visible.iter().map(|(line, owner)| {
            let line = markdown::fit_width(line, transcript_width);
            if focused && transcript_width > 1 && owner.map(|owner| owner.entry) == selected {
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
    if showing_transcript && state.scroll_offset > 0 {
        render_jump_button(&mut region, state, columns);
    } else {
        // The pill is the only transcript control that needs motion reports.
        // Drop the cell while it is hidden so the next appearance starts
        // unhovered until the pointer moves again.
        state.follow.pointer = None;
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
    frame.extend(activity_rows);
    let text_box_color = foreground_color(state.accent_color);
    let composer_label = if state.question.is_some() {
        Some("Your input")
    } else if plan_handoff {
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
            markdown::fit_composer_width(line, inner_width)
        ));
    }
    let bottom_border = match busy_composer_hint(state, editor, menu.is_empty(), inner_width) {
        Some(hint) => format!(
            "{}{hint}",
            "─".repeat(inner_width - markdown::visible_width(hint))
        ),
        None => "─".repeat(inner_width),
    };
    frame.push(format!("{text_box_color}└{bottom_border}┘\x1b[0m"));
    frame.extend(menu);

    if let Some(status_line) = status_line {
        frame.push(status_line);
    }

    if state.copy_toast_ticks > 0 {
        render_copy_toast(&mut frame, columns, state.accent_color);
    }

    // A focused picker field owns the cursor; the composer keeps its draft
    // untouched underneath. Frame rows and columns are one-based.
    let (cursor_row, cursor_col) = if let Some((row, column)) = picker_cursor {
        (row + 1, column + 1)
    } else if hide_input_cursor || plan_handoff {
        HIDDEN_CURSOR
    } else if let Some(query) = state.transcript.search_query() {
        (
            transcript_height + 1,
            (8 + markdown::visible_width(query)).min(columns.saturating_sub(1)),
        )
    } else {
        (
            transcript_height + search_height + activity_height + 2 + cursor_input_row,
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

/// Weighted RGB distance keeps activity separate from the composer accent.
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
    let Some(entry) = state.transcript.entry(index) else {
        state.transcript.close_viewer();
        return build_frame(state, &Editor::default(), columns, rows);
    };
    // The viewer reuses `scroll_offset` for its own content.
    state.follow.anchor = None;
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
        &labels,
        state.spinner_tick,
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
