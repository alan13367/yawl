//! Transcript cache and entry rendering.
//!
//! The frame builder coordinates each frame; this child caches per-entry
//! rendering across frames and builds transcript windows from the cache.

use std::sync::Arc;

use base64::Engine as _;

use crate::config::UiColor;
use crate::provider::{ImageContent, ReasoningKind};

use super::super::transcript::Entry;
use super::super::{USER_BACKGROUND, USER_TEXT, ViewState, markdown, tool_view};
use super::ImageSupport;
use super::foreground_color;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedImage {
    start_line: usize,
    columns: usize,
    rows: usize,
    content: Arc<ImageContent>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct RenderedEntry {
    pub(super) lines: Vec<String>,
    images: Vec<CachedImage>,
}

#[derive(Debug, Clone, Copy)]
struct RenderSettings {
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
    image_support: ImageSupport,
    accent_color: UiColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheSlot {
    width: usize,
    tools_expanded: bool,
    hide_reasoning: bool,
    image_support: ImageSupport,
    accent_color: UiColor,
    entries: Vec<Option<RenderedEntry>>,
    /// Absolute starting line for every entry, plus one total-height sentinel.
    entry_starts: Vec<usize>,
    frozen_count: usize,
}

impl CacheSlot {
    fn new(settings: RenderSettings) -> Self {
        Self {
            width: settings.width,
            tools_expanded: settings.tools_expanded,
            hide_reasoning: settings.hide_reasoning,
            image_support: settings.image_support,
            accent_color: settings.accent_color,
            entries: Vec::new(),
            entry_starts: vec![0],
            frozen_count: 0,
        }
    }

    fn matches(&self, settings: RenderSettings) -> bool {
        self.width == settings.width
            && self.tools_expanded == settings.tools_expanded
            && self.hide_reasoning == settings.hide_reasoning
            && self.image_support == settings.image_support
            && self.accent_color == settings.accent_color
    }

    fn update(
        &mut self,
        transcript: &super::super::transcript::Transcript,
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
        if self.entries.len() < entries.len() {
            changed_from = Some(self.entries.len());
            self.entries.resize_with(entries.len(), || None);
        } else if self.entries.len() > entries.len() {
            self.entries.truncate(entries.len());
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
            self.entries[i] = render_entry(
                entry,
                self.width,
                expanded,
                hide_reasoning,
                self.image_support,
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
            self.entries[idx] = render_entry(
                &entries[idx],
                self.width,
                expanded,
                hide_reasoning,
                self.image_support,
                self.accent_color,
                labels,
            );
            changed_from = Some(changed_from.map_or(idx, |changed| changed.min(idx)));
        }

        if let Some(start) = changed_from {
            self.entry_starts.resize(self.entries.len() + 1, 0);
            if start == 0 {
                self.entry_starts[0] = 0;
            }
            for index in start..self.entries.len() {
                let height = self.entries[index]
                    .as_ref()
                    .map_or(0, |entry| entry.lines.len() + 1);
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
            .min(self.entries.len().saturating_sub(1));
        let offset = row.saturating_sub(self.entry_starts[index]);
        let entry = self.entries.get(index)?.as_ref()?;
        if offset < entry.lines.len() {
            Some((entry.lines[offset].clone(), Some(index)))
        } else {
            Some((String::new(), None))
        }
    }

    fn images_in(
        &self,
        visible: std::ops::Range<usize>,
        screen_offset: usize,
    ) -> Vec<super::FrameImage> {
        let mut images = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let Some(entry) = entry else {
                continue;
            };
            let entry_start = self.entry_starts[index];
            for image in &entry.images {
                let start = entry_start + image.start_line;
                let end = start + image.rows;
                if start < visible.start || end > visible.end {
                    continue;
                }
                images.push(super::FrameImage {
                    key: Arc::as_ptr(&image.content) as usize,
                    row: screen_offset + start - visible.start + 1,
                    column: 2,
                    columns: image.columns,
                    rows: image.rows,
                    content: Arc::clone(&image.content),
                });
            }
        }
        images
    }

    #[cfg(test)]
    fn flattened(&self) -> Vec<String> {
        (0..self.total_lines())
            .filter_map(|row| self.line_at(row).map(|(line, _)| line))
            .collect()
    }
}

fn entry_default_expanded(entry: &Entry, tools_expanded: bool) -> bool {
    !matches!(entry, Entry::Tool { .. } | Entry::Diff { .. }) || tools_expanded
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(in crate::tui) struct RenderCache {
    slots: [Option<CacheSlot>; 2],
}

impl RenderCache {
    pub(in crate::tui) fn invalidate(&mut self) {
        self.slots = Default::default();
    }

    fn get_or_render_slot<'a>(
        &'a mut self,
        transcript: &super::super::transcript::Transcript,
        settings: RenderSettings,
        labels: &[(&str, &str)],
    ) -> &'a CacheSlot {
        let slot_idx = self
            .slots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|slot| slot.matches(settings)));

        let idx = match slot_idx {
            Some(i) => i,
            None => {
                let empty_idx = self.slots.iter().position(|s| s.is_none()).unwrap_or(1);
                self.slots[empty_idx] = Some(CacheSlot::new(settings));
                empty_idx
            }
        };

        let slot = self.slots[idx].as_mut().expect("slot was set above");
        slot.update(
            transcript,
            settings.tools_expanded,
            settings.hide_reasoning,
            labels,
        );
        slot
    }

    #[cfg(test)]
    pub(in crate::tui) fn get_or_render(
        &mut self,
        transcript: &super::super::transcript::Transcript,
        tools_expanded: bool,
        hide_reasoning: bool,
        accent_color: UiColor,
        width: usize,
    ) -> Vec<String> {
        self.get_or_render_slot(
            transcript,
            RenderSettings {
                width,
                tools_expanded,
                hide_reasoning,
                image_support: ImageSupport::None,
                accent_color,
            },
            &[],
        )
        .flattened()
    }
}

pub(super) fn render_entry(
    entry: &Entry,
    width: usize,
    expanded: bool,
    hide_reasoning: bool,
    image_support: ImageSupport,
    accent_color: UiColor,
    labels: &[(&str, &str)],
) -> Option<RenderedEntry> {
    let lines = match entry {
        Entry::User(content) if !expanded => render_collapsed("Prompt", content, width),
        Entry::User(content) => render_user_panel(content, width),
        Entry::Steer(content) if !expanded => render_collapsed("Steer", content, width),
        Entry::Steer(content) => render_user_panel(content, width),
        Entry::Assistant(content) => {
            if content.trim().is_empty() {
                return None;
            } else if !expanded {
                render_collapsed("Reply", content, width)
            } else {
                markdown::render(content.trim(), width)
            }
        }
        Entry::Reasoning { kind, content } => {
            if hide_reasoning || content.trim().is_empty() {
                return None;
            } else if !expanded {
                render_collapsed("Reasoning", content, width)
            } else {
                render_reasoning(*kind, content, width)
            }
        }
        Entry::Tool {
            name,
            args,
            output,
            images,
            is_error,
            running,
            started,
        } => {
            let mut lines = tool_view::render_labeled(
                name,
                args,
                output,
                *is_error,
                *running,
                started.map(|started| started.elapsed()),
                labels,
                width,
                expanded,
            );
            let previews = image_previews(images, image_support, width, lines.len());
            let reserved_rows = previews.iter().map(|image| image.rows).sum();
            lines.extend(std::iter::repeat_n(String::new(), reserved_rows));
            return Some(RenderedEntry {
                lines,
                images: previews,
            });
        }
        Entry::Notice(content) if !expanded => render_collapsed("Notice", content, width),
        Entry::Notice(content) => {
            let mut lines = vec![yawl_label(accent_color)];
            lines.extend(markdown::render(content, width));
            lines
        }
        Entry::Diff { path, lines } => tool_view::render_diff_card(path, lines, width, expanded),
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } if !expanded => render_collapsed(name, content, width),
        Entry::SubagentResult {
            id,
            name,
            status,
            content,
        } => render_subagent_result(id, name, status, content, width, true),
    };
    Some(RenderedEntry {
        lines,
        images: Vec::new(),
    })
}

fn image_previews(
    images: &[Arc<ImageContent>],
    support: ImageSupport,
    width: usize,
    mut start_line: usize,
) -> Vec<CachedImage> {
    let columns = width.saturating_sub(4).clamp(1, 100);
    images
        .iter()
        .filter(|image| support.accepts(&image.media_type))
        .filter_map(|image| {
            let max_encoded_bytes = crate::image::MAX_IMAGE_BYTES.div_ceil(3) * 4;
            if image.data.len() > max_encoded_bytes {
                return None;
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .ok()?;
            if bytes.len() > crate::image::MAX_IMAGE_BYTES
                || crate::image::media_type(&bytes) != Some(image.media_type.as_str())
            {
                return None;
            }
            let (pixel_width, pixel_height) = crate::image::dimensions(&bytes)?;
            let numerator = u64::from(pixel_height).saturating_mul(columns as u64);
            let denominator = u64::from(pixel_width).saturating_mul(2).max(1);
            let rows = usize::try_from(numerator.div_ceil(denominator))
                .unwrap_or(12)
                .clamp(3, 12);
            let preview = CachedImage {
                start_line,
                columns,
                rows,
                content: Arc::clone(image),
            };
            start_line += rows;
            Some(preview)
        })
        .collect()
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
pub(in crate::tui) fn render_entries(
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
            ImageSupport::None,
            UiColor::WHITE,
            &[],
        ) {
            lines.extend(rendered.lines);
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

pub(in crate::tui) fn render_reasoning(
    kind: ReasoningKind,
    content: &str,
    width: usize,
) -> Vec<String> {
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

pub(in crate::tui) fn render_user_panel(content: &str, width: usize) -> Vec<String> {
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

pub(in crate::tui) fn render_queued_panel(
    content: &str,
    position: usize,
    width: usize,
) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;33mQueued {position} · waiting for the active response\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

fn render_steer_panel(content: &str, position: usize, width: usize) -> Vec<String> {
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[2;36mSteer {position} · waiting for a safe boundary\x1b[0m"),
        width,
    )];
    lines.extend(render_user_panel(content, width));
    lines.push(String::new());
    lines
}

fn yawl_label(accent: UiColor) -> String {
    format!("{}\x1b[1mYawl\x1b[0m", foreground_color(accent))
}

pub(in crate::tui) struct TranscriptWindow {
    pub(in crate::tui) lines: Vec<(String, Option<usize>)>,
    pub(in crate::tui) images: Vec<super::FrameImage>,
    pub(in crate::tui) total_lines: usize,
    pub(in crate::tui) max_scroll: usize,
}

pub(super) fn subagent_labels(state: &ViewState) -> Vec<(String, String)> {
    state
        .subagent_snapshots
        .iter()
        .map(|snapshot| (snapshot.id.to_string(), snapshot.name.clone()))
        .collect()
}

pub(super) fn label_refs(labels: &[(String, String)]) -> Vec<(&str, &str)> {
    labels
        .iter()
        .map(|(id, name)| (id.as_str(), name.as_str()))
        .collect()
}

pub(in crate::tui) fn render_transcript_window(
    state: &mut ViewState,
    width: usize,
    height: usize,
    image_support: ImageSupport,
) -> TranscriptWindow {
    let mut tail = Vec::new();
    if let Some(loading) = render_loading_state(state, width) {
        tail.push(loading);
        tail.push(String::new());
    }
    for (index, input) in state.pending_steers.iter().enumerate() {
        tail.extend(render_steer_panel(&input.text, index + 1, width));
    }
    for (index, input) in state.queued_inputs.iter().enumerate() {
        tail.extend(render_queued_panel(&input.text, index + 1, width));
    }

    let selected = state.transcript.selected_index();
    let reveal = state.transcript.take_reveal_selected();
    let labels = subagent_labels(state);
    let label_refs = label_refs(&labels);
    let settings = RenderSettings {
        width,
        tools_expanded: state.tools_expanded,
        hide_reasoning: state.hide_reasoning,
        image_support,
        accent_color: state.accent_color,
    };
    let (cached_lines, selected_range) = {
        let slot = state
            .render_cache
            .get_or_render_slot(&state.transcript, settings, &label_refs);
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

    let slot = state
        .render_cache
        .get_or_render_slot(&state.transcript, settings, &label_refs);
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
    let cached_start = start.min(cached_lines);
    let cached_end = end.min(cached_lines);
    let screen_offset = height.saturating_sub(end - start);
    let images = if cached_start < cached_end {
        slot.images_in(cached_start..cached_end, screen_offset)
    } else {
        Vec::new()
    };
    TranscriptWindow {
        lines,
        images,
        total_lines,
        max_scroll,
    }
}

fn has_visible_in_flight_content(state: &ViewState) -> bool {
    let Some(last) = state.transcript.entries().last() else {
        return false;
    };
    match last {
        Entry::User(_)
        | Entry::Steer(_)
        | Entry::Notice(_)
        | Entry::Diff { .. }
        | Entry::SubagentResult { .. } => false,
        Entry::Tool { running, .. } => *running,
        Entry::Reasoning { content, .. } => !state.hide_reasoning && !content.trim().is_empty(),
        Entry::Assistant(content) => !content.trim().is_empty(),
    }
}

pub(in crate::tui) fn loading_label(activity: &str) -> Option<&str> {
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

pub(in crate::tui) fn render_loading_state(state: &ViewState, width: usize) -> Option<String> {
    let label = loading_label(&state.activity)?;
    if has_visible_in_flight_content(state)
        && !state.activity.starts_with("preparing ")
        && state.activity != "loading skill"
        && state.activity != "compacting conversation"
    {
        return None;
    }
    const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
    let accent = foreground_color(state.accent_color);
    let rendered = format!(" {accent}{frame}\x1b[0m \x1b[2m{label}\x1b[0m");
    Some(markdown::fit_width(&rendered, width))
}
