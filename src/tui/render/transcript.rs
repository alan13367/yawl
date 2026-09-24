//! Compose the visible transcript from cached entries and transient status/input rows.
//!
//! Owns scroll bounds, selection reveal, loading indicators, and screen placement.

use super::super::transcript::Entry;
use super::super::{ViewState, markdown};
use super::cache::{LineOwner, RenderSettings};
use super::entries::{render_queued_panel, render_steer_panel};
use super::{ImageSupport, SPINNER_FRAMES, foreground_color};

pub(in crate::tui) struct TranscriptWindow {
    pub(in crate::tui) lines: Vec<(String, Option<LineOwner>)>,
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
        accent_color: state.transcript_accent_color(),
    };
    let (cached_lines, selected_range) = {
        let slot = state.render_cache.get_or_render_slot(
            &state.transcript,
            settings,
            &label_refs,
            state.spinner_tick,
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
        settings,
        &label_refs,
        state.spinner_tick,
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
    let frame = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
    let accent = foreground_color(state.accent_color);
    let rendered = format!(" {accent}{frame}\x1b[0m \x1b[2m{label}\x1b[0m");
    Some(markdown::fit_width(&rendered, width))
}
