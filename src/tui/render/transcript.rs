//! Compose the visible transcript from cached entries and transient status/input rows.
//!
//! Owns scroll bounds, selection reveal, loading indicators, and screen placement.

use std::time::Duration;

use super::super::tool_view::SubagentLabel;
use super::super::transcript::Entry;
use super::super::{ViewState, markdown};
use super::cache::{LineOwner, RenderSettings};
use super::entries::{render_queued_panel, render_steer_panel};
use super::{ImageSupport, SPINNER_FRAMES, foreground_color};

/// How long a live response may sit without a model delta before `Waiting…`
/// returns. A server can buffer an entire tool call and emit nothing until
/// it is parsed; the preamble already on screen would otherwise freeze.
/// Long enough that ordinary pauses in slow or bursty streams do not flash
/// the spinner between deltas.
pub(in crate::tui) const MODEL_STREAM_STALL: Duration = Duration::from_secs(2);

pub(in crate::tui) struct TranscriptWindow {
    pub(in crate::tui) lines: Vec<(String, Option<LineOwner>)>,
    pub(in crate::tui) images: Vec<super::FrameImage>,
    pub(in crate::tui) total_lines: usize,
    pub(in crate::tui) max_scroll: usize,
}

pub(super) fn subagent_labels(state: &ViewState) -> Vec<SubagentLabel> {
    state
        .subagent_snapshots
        .iter()
        .map(|snapshot| SubagentLabel {
            id: snapshot.id.to_string(),
            name: snapshot.name.clone(),
            active: snapshot.status.is_active(),
        })
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
            &labels,
            state.spinner_tick,
        );
        (
            slot.total_lines(),
            selected.and_then(|index| slot.entry_range(index)),
        )
    };
    let total_lines = cached_lines + tail.len();
    let max_scroll = total_lines.saturating_sub(height);
    // While scrolled up, keep the same rows on screen as output grows or
    // shrinks below them; at the bottom (offset zero) the view follows.
    if state.scroll_offset > 0
        && let Some((anchor_width, anchor_total)) = state.follow.anchor
        && anchor_width == width
    {
        state.scroll_offset = if total_lines >= anchor_total {
            state.scroll_offset + (total_lines - anchor_total)
        } else {
            state
                .scroll_offset
                .saturating_sub(anchor_total - total_lines)
        };
    }
    state.follow.anchor = Some((width, total_lines));
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
        &labels,
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

/// What the newest transcript entry already shows of the response in flight.
enum InFlight {
    /// Nothing, so the loading row is the only progress cue.
    Nothing,
    /// Text that freezes when the stream goes quiet.
    Static,
    /// A running tool card or live thinking tag with its own spinner.
    Animated,
}

fn in_flight_content(state: &ViewState) -> InFlight {
    let Some(last) = state.transcript.entries().last() else {
        return InFlight::Nothing;
    };
    match last {
        Entry::User(_)
        | Entry::Steer(_)
        | Entry::Notice(_)
        | Entry::Diff { .. }
        | Entry::SubagentResult { .. } => InFlight::Nothing,
        Entry::Tool { running: true, .. } => InFlight::Animated,
        Entry::Tool { .. } => InFlight::Nothing,
        Entry::Reasoning { content, .. } if state.hide_reasoning || content.trim().is_empty() => {
            InFlight::Nothing
        }
        Entry::Reasoning { started, .. } if started.is_some() => InFlight::Animated,
        Entry::Reasoning { .. } => InFlight::Static,
        Entry::Assistant(content) if content.trim().is_empty() => InFlight::Nothing,
        Entry::Assistant(_) => InFlight::Static,
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
    let announces_step = state.activity.starts_with("preparing ")
        || state.activity.starts_with("attempt")
        || state.activity == "loading skill"
        || state.activity == "compacting conversation";
    let stream_quiet = state
        .transcript
        .last_model_activity()
        .is_some_and(|at| at.elapsed() >= MODEL_STREAM_STALL);
    let covered = match in_flight_content(state) {
        InFlight::Nothing => false,
        InFlight::Static => !stream_quiet,
        InFlight::Animated => true,
    };
    if covered && !announces_step {
        return None;
    }
    let frame = SPINNER_FRAMES[state.spinner_tick % SPINNER_FRAMES.len()];
    let accent = foreground_color(state.accent_color);
    let rendered = format!(" {accent}{frame}\x1b[0m \x1b[2m{label}\x1b[0m");
    Some(markdown::fit_width(&rendered, width))
}
