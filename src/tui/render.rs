//! Full-screen frame composition and transcript presentation.

use crate::config::UiColor;
use crate::provider::ReasoningKind;

use super::completion::{
    COMPLETION_MENU_ROWS, completion_window, menu_rows, sync_completion_filter,
};
use super::input::Editor;
use super::picker::render_picker;
use super::state::{ScrollGeometry, scroll_bar_position, scroll_bar_span};
use super::transcript::Entry;
use super::{USER_BACKGROUND, USER_TEXT, ViewState, markdown, tool_view};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheSlot {
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
    entry_lines: Vec<Option<Vec<String>>>,
    flattened: Vec<String>,
    frozen_count: usize,
}

impl CacheSlot {
    fn new(width: usize, tools_expanded: bool, hide_reasoning: bool) -> Self {
        Self {
            width,
            tools_expanded,
            hide_reasoning,
            entry_lines: Vec::new(),
            flattened: Vec::new(),
            frozen_count: 0,
        }
    }

    fn matches(&self, width: usize, tools_expanded: bool, hide_reasoning: bool) -> bool {
        self.width == width
            && self.tools_expanded == tools_expanded
            && self.hide_reasoning == hide_reasoning
    }

    fn update(
        &mut self,
        transcript: &super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
    ) {
        let entries = transcript.entries();
        if entries.len() < self.frozen_count {
            self.frozen_count = entries.len();
        }

        let mutable_index = transcript
            .streaming_index()
            .or_else(|| transcript.running_tool_index());
        let frozen_boundary = mutable_index.unwrap_or(entries.len()).min(entries.len());

        let mut changed = false;
        if self.entry_lines.len() < entries.len() {
            self.entry_lines.resize_with(entries.len(), || None);
        } else if self.entry_lines.len() > entries.len() {
            self.entry_lines.truncate(entries.len());
            changed = true;
        }

        for (i, entry) in entries
            .iter()
            .enumerate()
            .take(frozen_boundary)
            .skip(self.frozen_count)
        {
            self.entry_lines[i] = render_entry(entry, self.width, tools_expanded, hide_reasoning);
            changed = true;
        }
        self.frozen_count = frozen_boundary;

        if let Some(idx) = mutable_index
            && idx < entries.len()
        {
            self.entry_lines[idx] =
                render_entry(&entries[idx], self.width, tools_expanded, hide_reasoning);
            changed = true;
        }

        if changed || (self.flattened.is_empty() && !entries.is_empty()) {
            self.flattened.clear();
            for lines in self.entry_lines.iter().flatten() {
                self.flattened.extend(lines.iter().cloned());
                self.flattened.push(String::new());
            }
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RenderCache {
    slots: [Option<CacheSlot>; 2],
}

impl RenderCache {
    pub(super) fn invalidate(&mut self) {
        self.slots = Default::default();
    }

    pub(super) fn get_or_render<'a>(
        &'a mut self,
        transcript: &super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        width: usize,
    ) -> &'a [String] {
        let slot_idx = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|s| s.matches(width, tools_expanded, hide_reasoning))
        });

        let idx = match slot_idx {
            Some(i) => i,
            None => {
                let empty_idx = self.slots.iter().position(|s| s.is_none()).unwrap_or(1);
                self.slots[empty_idx] = Some(CacheSlot::new(width, tools_expanded, hide_reasoning));
                empty_idx
            }
        };

        let slot = self.slots[idx].as_mut().expect("slot was set above");
        slot.update(transcript, tools_expanded, hide_reasoning);
        &slot.flattened
    }
}

fn render_entry(
    entry: &Entry,
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
) -> Option<Vec<String>> {
    match entry {
        Entry::User(content) => Some(render_user_panel(content, width)),
        Entry::Assistant(content) => {
            if content.trim().is_empty() {
                None
            } else {
                Some(markdown::render(content.trim(), width))
            }
        }
        Entry::Reasoning { kind, content } => {
            if hide_reasoning || content.trim().is_empty() {
                None
            } else {
                Some(render_reasoning(*kind, content, width))
            }
        }
        Entry::Tool {
            name,
            args,
            output,
            is_error,
            running,
        } => Some(tool_view::render(
            name,
            args,
            output,
            *is_error,
            *running,
            width,
            tools_expanded,
        )),
        Entry::Notice(content) => {
            let mut lines = vec!["\x1b[1;33mYawl\x1b[0m".into()];
            lines.extend(markdown::render(content, width));
            Some(lines)
        }
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } => Some(render_subagent_result(
            id,
            name,
            status,
            content,
            width,
            tools_expanded,
        )),
    }
}

#[cfg(test)]
pub(super) fn render_entries(
    entries: &[Entry],
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    for entry in entries {
        if let Some(rendered) = render_entry(entry, width, tools_expanded, hide_reasoning) {
            lines.extend(rendered);
            lines.push(String::new());
        }
    }
    lines
}

fn render_subagent_result(
    id: &str,
    name: &str,
    status: &str,
    content: &str,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let id = crate::subagent::sanitize_preview(id, 256);
    let name = crate::subagent::sanitize_preview(name, 1024);
    let status = crate::subagent::sanitize_preview(status, 64);
    let color = if status == "failed" || status == "interrupted" {
        "\x1b[1;31m"
    } else {
        "\x1b[1;36m"
    };
    let mut lines = vec![markdown::fit_width(
        &format!("{color}Subagent {id} [{status}] {name}\x1b[0m"),
        width,
    )];
    let rendered = markdown::render(content, width);
    if expanded || rendered.len() <= 8 {
        lines.extend(rendered);
    } else {
        lines.extend(rendered.into_iter().take(8));
        lines.push(markdown::fit_width(
            "\x1b[2m… Ctrl+O to expand\x1b[0m",
            width,
        ));
    }
    lines
}

pub(super) fn render_reasoning(kind: ReasoningKind, content: &str, width: usize) -> Vec<String> {
    const STYLE: &str = "\x1b[2;3;38;2;148;148;158m";
    let continuation = format!("\x1b[0m{STYLE}");
    let style = |line: String| format!("{STYLE}{}\x1b[0m", line.replace("\x1b[0m", &continuation));
    match kind {
        ReasoningKind::Summary => reasoning_summary_parts(content)
            .into_iter()
            .flat_map(|summary| markdown::render(&summary, width))
            .map(style)
            .collect(),
        ReasoningKind::Full => markdown::render(content.trim(), width)
            .into_iter()
            .map(style)
            .collect(),
    }
}

fn reasoning_summary_parts(content: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
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

fn render_transcript(state: &mut ViewState, width: usize) -> Vec<String> {
    let mut transcript = state
        .render_cache
        .get_or_render(
            &state.transcript,
            state.tools_expanded,
            state.hide_reasoning,
            width,
        )
        .to_vec();
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
        Entry::User(_) | Entry::Notice(_) | Entry::SubagentResult { .. } => false,
        Entry::Tool { running, .. } => *running,
        Entry::Reasoning { content, .. } => !state.hide_reasoning && !content.trim().is_empty(),
        Entry::Assistant(content) => !content.trim().is_empty(),
    }
}

pub(super) fn loading_label(activity: &str) -> Option<&str> {
    match activity {
        "sending" | "responding" | "reasoning" => Some("Waiting…"),
        "preparing write" => Some("Preparing write…"),
        "preparing edit" => Some("Preparing edit…"),
        "preparing tool" => Some("Preparing tool…"),
        "compacting conversation" => Some("Compacting conversation…"),
        "canceling turn" => Some("Canceling turn…"),
        other if other.starts_with("attempt") => Some(other),
        _ => None,
    }
}

pub(super) fn render_loading_state(state: &ViewState, width: usize) -> Option<String> {
    let label = loading_label(&state.activity)?;
    if has_visible_in_flight_content(state) && !state.activity.starts_with("preparing ") {
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
    if state.subagent_view.is_some() {
        return super::subagents::render(state, editor, columns, rows);
    }
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
    let menu_capacity = COMPLETION_MENU_ROWS.min(rows.saturating_sub(input_height + 1));
    let menu_entries = if state.picker.is_none() {
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
        region.extend(render_picker(
            picker,
            editor,
            &selection_style(state.selection_color),
            columns,
            transcript_height,
        ));
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
    frame.extend(menu);

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
        "{}  {}/{} tokens ({}%)",
        reasoning, state.context_tokens, state.context_window, percentage
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
    if !state.subagent_snapshots.is_empty() {
        let running = state
            .subagent_snapshots
            .iter()
            .filter(|snapshot| snapshot.status.is_active())
            .count();
        let done = state
            .subagent_snapshots
            .iter()
            .filter(|snapshot| snapshot.status == crate::subagent::SubagentStatus::Done)
            .count();
        let failed = state
            .subagent_snapshots
            .iter()
            .filter(|snapshot| snapshot.status == crate::subagent::SubagentStatus::Failed)
            .count();
        status.push_str(&format!(
            "  agents {running} running · {done} done · {failed} failed"
        ));
        if state.subagent_tokens > 0 {
            status.push_str(&format!(
                " · {} child tokens",
                format_token_count(state.subagent_tokens)
            ));
        }
    }
    frame.push(markdown::fit_width(
        &format!(
            " {}\x1b[1m{}\x1b[22m{}{status}\x1b[0m",
            foreground_color(state.accent_color),
            state.model,
            status_style(state.accent_color),
        ),
        columns,
    ));

    if state.copy_toast_ticks > 0 {
        render_copy_toast(&mut frame, columns, state.accent_color);
    }

    let cursor_row = transcript_height + 2 + cursor_input_row;
    let cursor_col = (2 + layout.cursor_col).min(columns.saturating_sub(1));
    (frame, (cursor_row, cursor_col))
}

/// Compact token counts for the status bar: raw below 10,000, then 12.3k
/// and 1.2M steps so the line stays short.
pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens < 10_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        return format!("{}.{:01}k", tokens / 1_000, tokens % 1_000 / 100);
    }
    format!(
        "{}.{:01}M",
        tokens / 1_000_000,
        tokens % 1_000_000 / 100_000
    )
}
