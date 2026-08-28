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

const BACKGROUND_NOTICE_CYAN: UiColor = UiColor::new(116, 199, 213);
const BACKGROUND_NOTICE_AMBER: UiColor = UiColor::new(232, 202, 118);

/// Invalid one-based terminal coordinates signal views that have no editor
/// and must leave the hardware cursor hidden.
pub(super) const HIDDEN_CURSOR: (usize, usize) = (0, 0);

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheSlot {
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
    accent_color: UiColor,
    entry_lines: Vec<Option<Vec<String>>>,
    /// Absolute starting line for every entry, plus one total-height sentinel.
    entry_starts: Vec<usize>,
    frozen_count: usize,
}

impl CacheSlot {
    fn new(
        width: usize,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
    ) -> Self {
        Self {
            width,
            tools_expanded,
            hide_reasoning,
            accent_color,
            entry_lines: Vec::new(),
            entry_starts: vec![0],
            frozen_count: 0,
        }
    }

    fn matches(
        &self,
        width: usize,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
    ) -> bool {
        self.width == width
            && self.tools_expanded == tools_expanded
            && self.hide_reasoning == hide_reasoning
            && self.accent_color == accent_color
    }

    fn update(
        &mut self,
        transcript: &super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        labels: &[(&str, &str)],
    ) {
        let entries = transcript.entries();
        if entries.len() < self.frozen_count {
            self.frozen_count = entries.len();
        }

        let mutable_index = transcript
            .streaming_index()
            .or_else(|| transcript.running_tool_index());
        let frozen_boundary = mutable_index.unwrap_or(entries.len()).min(entries.len());

        let mut changed_from = None;
        if self.entry_lines.len() < entries.len() {
            changed_from = Some(self.entry_lines.len());
            self.entry_lines.resize_with(entries.len(), || None);
        } else if self.entry_lines.len() > entries.len() {
            self.entry_lines.truncate(entries.len());
            changed_from = Some(entries.len());
        }

        for (i, entry) in entries
            .iter()
            .enumerate()
            .take(frozen_boundary)
            .skip(self.frozen_count)
        {
            let expanded =
                transcript.entry_expanded(i, entry_default_expanded(entry, tools_expanded));
            self.entry_lines[i] = render_entry(
                entry,
                self.width,
                expanded,
                hide_reasoning,
                self.accent_color,
                labels,
            );
            changed_from = Some(changed_from.map_or(i, |changed| changed.min(i)));
        }
        self.frozen_count = frozen_boundary;

        if let Some(idx) = mutable_index
            && idx < entries.len()
        {
            let expanded = transcript
                .entry_expanded(idx, entry_default_expanded(&entries[idx], tools_expanded));
            self.entry_lines[idx] = render_entry(
                &entries[idx],
                self.width,
                expanded,
                hide_reasoning,
                self.accent_color,
                labels,
            );
            changed_from = Some(changed_from.map_or(idx, |changed| changed.min(idx)));
        }

        if let Some(start) = changed_from {
            self.entry_starts.resize(self.entry_lines.len() + 1, 0);
            if start == 0 {
                self.entry_starts[0] = 0;
            }
            for index in start..self.entry_lines.len() {
                let height = self.entry_lines[index]
                    .as_ref()
                    .map_or(0, |lines| lines.len() + 1);
                self.entry_starts[index + 1] = self.entry_starts[index] + height;
            }
        }
    }

    fn total_lines(&self) -> usize {
        self.entry_starts.last().copied().unwrap_or(0)
    }

    fn entry_range(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let start = *self.entry_starts.get(index)?;
        let end = *self.entry_starts.get(index + 1)?;
        (start < end).then_some(start..end)
    }

    fn line_at(&self, row: usize) -> Option<(String, Option<usize>)> {
        if row >= self.total_lines() {
            return None;
        }
        let index = self
            .entry_starts
            .partition_point(|start| *start <= row)
            .saturating_sub(1)
            .min(self.entry_lines.len().saturating_sub(1));
        let offset = row.saturating_sub(self.entry_starts[index]);
        let lines = self.entry_lines.get(index)?.as_ref()?;
        if offset < lines.len() {
            Some((lines[offset].clone(), Some(index)))
        } else {
            Some((String::new(), None))
        }
    }

    #[cfg(test)]
    fn flattened(&self) -> Vec<String> {
        (0..self.total_lines())
            .filter_map(|row| self.line_at(row).map(|(line, _)| line))
            .collect()
    }
}

fn entry_default_expanded(entry: &Entry, tools_expanded: bool) -> bool {
    !matches!(entry, Entry::Tool { .. }) || tools_expanded
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RenderCache {
    slots: [Option<CacheSlot>; 2],
}

impl RenderCache {
    pub(super) fn invalidate(&mut self) {
        self.slots = Default::default();
    }

    fn get_or_render_slot<'a>(
        &'a mut self,
        transcript: &super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
        width: usize,
        labels: &[(&str, &str)],
    ) -> &'a CacheSlot {
        let slot_idx = self.slots.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|s| s.matches(width, tools_expanded, hide_reasoning, accent_color))
        });

        let idx = match slot_idx {
            Some(i) => i,
            None => {
                let empty_idx = self.slots.iter().position(|s| s.is_none()).unwrap_or(1);
                self.slots[empty_idx] = Some(CacheSlot::new(
                    width,
                    tools_expanded,
                    hide_reasoning,
                    accent_color,
                ));
                empty_idx
            }
        };

        let slot = self.slots[idx].as_mut().expect("slot was set above");
        slot.update(transcript, tools_expanded, hide_reasoning, labels);
        slot
    }

    #[cfg(test)]
    pub(super) fn get_or_render(
        &mut self,
        transcript: &super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
        width: usize,
    ) -> Vec<String> {
        self.get_or_render_slot(
            transcript,
            tools_expanded,
            hide_reasoning,
            accent_color,
            width,
            &[],
        )
        .flattened()
    }
}

fn render_entry(
    entry: &Entry,
    width: usize,
    expanded: bool,
    hide_reasoning: bool,
    accent_color: UiColor,
    labels: &[(&str, &str)],
) -> Option<Vec<String>> {
    match entry {
        Entry::User(content) if !expanded => Some(render_collapsed("Prompt", content, width)),
        Entry::User(content) => Some(render_user_panel(content, width)),
        Entry::Assistant(content) => {
            if content.trim().is_empty() {
                None
            } else if !expanded {
                Some(render_collapsed("Reply", content, width))
            } else {
                Some(markdown::render(content.trim(), width))
            }
        }
        Entry::Reasoning { kind, content } => {
            if hide_reasoning || content.trim().is_empty() {
                None
            } else if !expanded {
                Some(render_collapsed("Reasoning", content, width))
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
            started,
        } => Some(tool_view::render_labeled(
            name,
            args,
            output,
            *is_error,
            *running,
            started.map(|started| started.elapsed()),
            labels,
            width,
            expanded,
        )),
        Entry::Notice(content) if !expanded => Some(render_collapsed("Notice", content, width)),
        Entry::Notice(content) => {
            let mut lines = vec![yawl_label(accent_color)];
            lines.extend(markdown::render(content, width));
            Some(lines)
        }
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } if !expanded => Some(render_collapsed(name, content, width)),
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } => Some(render_subagent_result(
            id, name, status, content, width, true,
        )),
    }
}

fn render_collapsed(label: &str, content: &str, width: usize) -> Vec<String> {
    let preview = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let preview = markdown::render(preview, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let label = crate::subagent::sanitize_preview(label, 128);
    vec![markdown::fit_width(
        &format!("\x1b[2m▸ {label}\x1b[0m  {preview}"),
        width,
    )]
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
        if let Some(rendered) = render_entry(
            entry,
            width,
            entry_default_expanded(entry, tools_expanded),
            hide_reasoning,
            UiColor::WHITE,
            &[],
        ) {
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

fn yawl_label(accent: UiColor) -> String {
    format!("{}\x1b[1mYawl\x1b[0m", foreground_color(accent))
}

/// Block letters for typical terminal widths. Falls back to the smaller
/// figlet wordmark, then to the word itself, when the region is tight.
const WELCOME_LOGO_LARGE: &[&str] = &[
    "██    ██   █████   ██     ██  ██",
    " ██  ██   ██   ██  ██     ██  ██",
    "  ████    ███████  ██  █  ██  ██",
    "   ██     ██   ██  ██ ███ ██  ██",
    "   ██     ██   ██   ███ ███   ███████",
];

const WELCOME_LOGO_SMALL: &[&str] = &[
    r"__   __            _",
    r"\ \ / /_ ___      _| |",
    r" \ V / _` \ \ /\ / / |",
    r"  | | (_| |\ V  V /| |",
    r"  |_|\__,_| \_/\_/ |_|",
];

fn logo_fits(lines: &[&str], width: usize) -> bool {
    lines
        .iter()
        .map(|line| markdown::visible_width(line))
        .max()
        .unwrap_or(0)
        <= width
}

fn welcome_logo(width: usize, height: usize) -> &'static [&'static str] {
    const COMPACT: &[&str] = &["Yawl"];
    if height >= WELCOME_LOGO_LARGE.len() && logo_fits(WELCOME_LOGO_LARGE, width) {
        WELCOME_LOGO_LARGE
    } else if height >= WELCOME_LOGO_SMALL.len() && logo_fits(WELCOME_LOGO_SMALL, width) {
        WELCOME_LOGO_SMALL
    } else {
        COMPACT
    }
}

/// Columns of the wordmark (and hint characters) revealed per 100ms tick.
const WELCOME_COLUMNS_PER_TICK: usize = 3;

/// Ticks after which the large wordmark and hint have finished typing.
pub(super) const WELCOME_ANIMATION_TICKS: usize = 40;

fn pad_to_width(text: &str, width: usize) -> String {
    let visible = markdown::visible_width(text);
    if visible >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - visible))
    }
}

fn center_styled(text: &str, style: &str, width: usize) -> String {
    let visible = markdown::visible_width(text);
    let left = width.saturating_sub(visible) / 2;
    markdown::fit_width(&format!("{}{style}{text}\x1b[0m", " ".repeat(left)), width)
}

fn prefix_visible(text: &str, columns: usize) -> &str {
    if columns == 0 {
        return "";
    }
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if markdown::visible_width(&text[..next]) > columns {
            break;
        }
        end = next;
    }
    &text[..end]
}

fn typed_amount(tick: usize, total: usize) -> usize {
    tick.saturating_add(1)
        .saturating_mul(WELCOME_COLUMNS_PER_TICK)
        .min(total)
}

fn typed_hint(tick: usize, logo_ticks: usize, hint: &str) -> &str {
    let Some(elapsed) = tick.saturating_add(1).checked_sub(logo_ticks) else {
        return "";
    };
    if elapsed == 0 {
        return "";
    }
    let keep = typed_amount(elapsed - 1, hint.chars().count());
    hint.char_indices()
        .nth(keep)
        .map_or(hint, |(index, _)| &hint[..index])
}

fn mask_logo_line(padded: &str, revealed: usize, show_cursor: bool) -> String {
    let total = markdown::visible_width(padded);
    if revealed >= total {
        return padded.to_string();
    }
    let mut line = String::with_capacity(padded.len() + 1);
    line.push_str(prefix_visible(padded, revealed));
    let width = markdown::visible_width(&line);
    if width < revealed {
        line.extend(std::iter::repeat_n(' ', revealed - width));
    }
    if show_cursor {
        line.push('|');
    }
    pad_to_width(&line, total)
}

fn render_welcome(accent: UiColor, width: usize, height: usize, tick: usize) -> Vec<String> {
    let color = format!("{}\x1b[1m", foreground_color(accent));
    let logo = welcome_logo(width, height);
    let block_width = logo
        .iter()
        .map(|line| markdown::visible_width(line))
        .max()
        .unwrap_or(0);
    let revealed = typed_amount(tick, block_width);
    let show_cursor = revealed < block_width && tick.is_multiple_of(2);
    let mut content: Vec<String> = logo
        .iter()
        .map(|line| {
            let padded = pad_to_width(line, block_width);
            let typed = mask_logo_line(&padded, revealed, show_cursor);
            center_styled(&typed, &color, width)
        })
        .collect();
    const HINT: &str = "Type /help for commands.";
    if height >= content.len() + 2 && markdown::visible_width(HINT) <= width {
        let blank = markdown::fit_width("", width);
        content.push(blank.clone());
        let logo_ticks = block_width.div_ceil(WELCOME_COLUMNS_PER_TICK);
        let hint = typed_hint(tick, logo_ticks, HINT);
        if hint.is_empty() {
            content.push(blank);
        } else {
            content.push(center_styled(hint, &status_style(accent), width));
        }
    }
    let blank = markdown::fit_width("", width);
    let top = height.saturating_sub(content.len()) / 2;
    let mut lines = Vec::with_capacity(height);
    lines.resize(top, blank.clone());
    lines.extend(content);
    lines.resize(height, blank);
    lines
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
        || state.picker.is_some()
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

struct TranscriptWindow {
    lines: Vec<(String, Option<usize>)>,
    total_lines: usize,
    max_scroll: usize,
}

fn subagent_labels(state: &ViewState) -> Vec<(String, String)> {
    state
        .subagent_snapshots
        .iter()
        .map(|snapshot| (snapshot.id.to_string(), snapshot.name.clone()))
        .collect()
}

fn label_refs(labels: &[(String, String)]) -> Vec<(&str, &str)> {
    labels
        .iter()
        .map(|(id, name)| (id.as_str(), name.as_str()))
        .collect()
}

fn render_transcript_window(
    state: &mut ViewState,
    width: usize,
    height: usize,
) -> TranscriptWindow {
    let mut tail = Vec::new();
    if let Some(loading) = render_loading_state(state, width) {
        tail.push(loading);
        tail.push(String::new());
    }
    for (index, input) in state.queued_inputs.iter().enumerate() {
        tail.extend(render_queued_panel(input, index + 1, width));
    }

    let selected = state.transcript.selected_index();
    let reveal = state.transcript.take_reveal_selected();
    let labels = subagent_labels(state);
    let label_refs = label_refs(&labels);
    let (cached_lines, selected_range) = {
        let slot = state.render_cache.get_or_render_slot(
            &state.transcript,
            state.tools_expanded,
            state.hide_reasoning,
            state.accent_color,
            width,
            &label_refs,
        );
        (
            slot.total_lines(),
            selected.and_then(|index| slot.entry_range(index)),
        )
    };
    let total_lines = cached_lines + tail.len();
    let max_scroll = total_lines.saturating_sub(height);
    if reveal && let Some(range) = selected_range {
        state.scroll_offset = max_scroll.saturating_sub(range.start);
    } else {
        state.scroll_offset = state.scroll_offset.min(max_scroll);
    }
    let end = total_lines.saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(height);

    let slot = state.render_cache.get_or_render_slot(
        &state.transcript,
        state.tools_expanded,
        state.hide_reasoning,
        state.accent_color,
        width,
        &label_refs,
    );
    let mut lines = Vec::with_capacity(end - start);
    for row in start..end {
        if row < cached_lines {
            if let Some(line) = slot.line_at(row) {
                lines.push(line);
            }
        } else if let Some(line) = tail.get(row - cached_lines) {
            lines.push((line.clone(), None));
        }
    }
    TranscriptWindow {
        lines,
        total_lines,
        max_scroll,
    }
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
        "loading skill" => Some("Loading skill…"),
        "compacting conversation" => Some("Compacting conversation…"),
        "canceling turn" => Some("Canceling turn…"),
        other if other.starts_with("attempt") => Some(other),
        _ => None,
    }
}

pub(super) fn render_loading_state(state: &ViewState, width: usize) -> Option<String> {
    let label = loading_label(&state.activity)?;
    if has_visible_in_flight_content(state)
        && !state.activity.starts_with("preparing ")
        && state.activity != "loading skill"
    {
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
    if state.process_view.is_some() {
        return super::processes::render(state, columns, rows);
    }
    if state.subagent_view.is_some() {
        return super::subagents::render(state, editor, columns, rows);
    }
    let columns = columns.max(20);
    let rows = rows.max(8);
    if state.transcript.viewer_open() {
        return render_block_viewer(state, columns, rows);
    }
    let inner_width = columns.saturating_sub(2);
    let layout = if super::picker::picker_is_secret(state) {
        editor.masked_layout(inner_width)
    } else {
        editor.layout(inner_width)
    };
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
    let background_count = state.background_processes.active_count();
    state.background_active_count = background_count;
    let background_notice_height = usize::from(background_count > 0);
    let menu_capacity =
        COMPLETION_MENU_ROWS.min(rows.saturating_sub(input_height + background_notice_height + 1));
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
    let search_height = usize::from(state.transcript.search_active());
    let transcript_height = rows
        .saturating_sub(input_height + menu_height + search_height + background_notice_height + 1);
    let transcript = render_transcript_window(state, columns, transcript_height);
    let transcript_width = columns;
    let visible = &transcript.lines;

    let mut region = Vec::with_capacity(transcript_height);
    if let Some(picker) = &state.picker {
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

    if background_count > 0 {
        frame.push(render_background_process_notice(
            background_count,
            columns,
            state.accent_color,
        ));
    }

    let percentage = state
        .context_tokens
        .saturating_mul(100)
        .checked_div(state.context_window)
        .unwrap_or(0);
    let mut parts = Vec::new();
    if let Some(effort) = state
        .reasoning_effort
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        parts.push(effort.to_string());
    }
    parts.push(format!(
        "{}% / {}",
        percentage,
        format_token_count(state.context_window)
    ));
    if let Some(started) = state.turn_started {
        parts.push(tool_view::format_elapsed(started.elapsed()));
    }
    if !state.queued_inputs.is_empty() {
        parts.push(format!("{} queued", state.queued_inputs.len()));
    }
    if !state.pending_actions.is_empty() {
        parts.push(format!("{} pending", state.pending_actions.len()));
    }
    parts.extend(agent_status_parts(state));
    let status = if parts.is_empty() {
        String::new()
    } else {
        format!("  ·  {}", parts.join("  ·  "))
    };
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

    let (cursor_row, cursor_col) = if let Some(query) = state.transcript.search_query() {
        (
            transcript_height + 1,
            (8 + markdown::visible_width(query)).min(columns.saturating_sub(1)),
        )
    } else {
        (
            transcript_height + search_height + 2 + cursor_input_row,
            (2 + layout.cursor_col).min(columns.saturating_sub(1)),
        )
    };
    (frame, (cursor_row, cursor_col))
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
        state.accent_color,
        &label_refs,
    )
    .unwrap_or_default();
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

fn agent_status_parts(state: &ViewState) -> Vec<String> {
    if state.subagent_snapshots.is_empty() && state.subagent_tokens == 0 {
        return Vec::new();
    }
    let running = state
        .subagent_snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active())
        .collect::<Vec<_>>();
    let failed = state
        .subagent_snapshots
        .iter()
        .filter(|snapshot| snapshot.status == crate::subagent::SubagentStatus::Failed)
        .count();
    let mut parts = Vec::new();
    match running.as_slice() {
        [] => {}
        [snapshot] => parts.push(snapshot.name.clone()),
        many => parts.push(format!("{} agents", many.len())),
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if state.subagent_tokens > 0 {
        parts.push(format!(
            "{} child",
            format_token_count(state.subagent_tokens)
        ));
    }
    parts
}

/// Compact token counts for the status bar: raw below 10,000, then 12.3k
/// and 1.2M steps so the line stays short. Whole thousands drop the
/// trailing `.0` (`400k`, `1M`).
pub(super) fn format_token_count(tokens: u64) -> String {
    if tokens < 10_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let whole = tokens / 1_000;
        let tenths = tokens % 1_000 / 100;
        if tenths == 0 {
            return format!("{whole}k");
        }
        return format!("{whole}.{tenths}k");
    }
    let whole = tokens / 1_000_000;
    let tenths = tokens % 1_000_000 / 100_000;
    if tenths == 0 {
        return format!("{whole}M");
    }
    format!("{whole}.{tenths}M")
}
