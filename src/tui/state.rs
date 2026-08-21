//! Mutable UI state and owned agent-event updates.

use crate::agent::{Agent, TurnEvent};
use crate::config::UiColor;

use super::completion::{Completion, command_completions};
use super::events::{MouseEvent, MouseKind};
use super::picker::{Picker, PickerAction};
use super::transcript::{Transcript, TranscriptEvent};

pub(super) const COPY_TOAST_TICKS: u8 = 15;

/// Layout facts the scroll bar needs to render and hit-test. Captured each
/// frame while the bar is drawn.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScrollGeometry {
    pub(super) rows: usize,
    pub(super) columns: usize,
    pub(super) max_scroll: usize,
    pub(super) travel: usize,
    pub(super) thumb_length: usize,
}

pub(super) struct ViewState {
    pub(super) transcript: Transcript,
    pub(super) tools_expanded: bool,
    pub(super) model: String,
    pub(super) reasoning_effort: Option<String>,
    pub(super) hide_reasoning: bool,
    pub(super) accent_color: UiColor,
    pub(super) show_scroll_bar: bool,
    pub(super) scroll_geometry: Option<ScrollGeometry>,
    pub(super) scroll_bar_drag: Option<usize>,
    pub(super) copy_toast_ticks: u8,
    pub(super) spinner_tick: usize,
    pub(super) context_tokens: u64,
    pub(super) context_window: u64,
    pub(super) activity: String,
    pub(super) scroll_offset: usize,
    pub(super) queued_inputs: std::collections::VecDeque<String>,
    pub(super) pending_actions: std::collections::VecDeque<PickerAction>,
    pub(super) completions: Vec<Completion>,
    pub(super) completion_index: usize,
    pub(super) picker: Option<Picker>,
}

impl ViewState {
    pub(super) fn from_agent(agent: &Agent) -> Self {
        Self {
            transcript: Transcript::from_messages(agent.messages()),
            tools_expanded: false,
            model: agent.model().to_string(),
            reasoning_effort: agent.config().reasoning_effort.clone(),
            hide_reasoning: agent.config().hide_reasoning,
            accent_color: agent.config().accent_color,
            show_scroll_bar: agent.config().scroll_bar,
            scroll_geometry: None,
            scroll_bar_drag: None,
            copy_toast_ticks: 0,
            spinner_tick: 0,
            context_tokens: agent.context_tokens(),
            context_window: agent.context_window(),
            activity: String::new(),
            scroll_offset: 0,
            queued_inputs: std::collections::VecDeque::new(),
            pending_actions: std::collections::VecDeque::new(),
            completions: command_completions(agent),
            completion_index: 0,
            picker: None,
        }
    }

    pub(super) fn refresh_completions(&mut self, agent: &Agent) {
        self.completions = command_completions(agent);
        self.completion_index = 0;
    }

    pub(super) fn notice(&mut self, text: impl Into<String>) {
        self.transcript.notice(text.into());
        self.scroll_offset = 0;
    }

    pub(super) fn apply(&mut self, update: Update) {
        let follow_bottom = self.scroll_offset == 0;
        match update {
            Update::Transcript(event) => {
                self.activity = match &event {
                    TranscriptEvent::TextDelta(_) => "responding".into(),
                    TranscriptEvent::ReasoningDelta { .. } if !self.hide_reasoning => {
                        "reasoning".into()
                    }
                    TranscriptEvent::ReasoningDelta { .. } => "responding".into(),
                    TranscriptEvent::ToolStart { .. } => "running tool".into(),
                    TranscriptEvent::AssistantDone => String::new(),
                    TranscriptEvent::ToolEnd { .. } => "sending".into(),
                    TranscriptEvent::RetryReset => self.activity.clone(),
                };
                self.transcript.apply(event);
            }
            Update::Retrying {
                attempt,
                delay_ms,
                error,
            } => {
                self.activity = format!(
                    "attempt {attempt} failed, retrying in {delay_ms}ms: {}",
                    crate::error::truncate(&error, 80)
                );
            }
            Update::Compacting => self.activity = "compacting conversation".into(),
            Update::Compacted { replaced } => {
                self.activity.clear();
                self.notice(format!("Compacted {replaced} older messages."));
            }
            Update::Warning(text) => {
                self.activity.clear();
                self.notice(text);
            }
            Update::Usage {
                context_tokens,
                context_window,
            } => {
                self.context_tokens = context_tokens;
                self.context_window = context_window;
            }
        }
        if follow_bottom {
            self.scroll_offset = 0;
        }
    }
}

pub(super) enum Update {
    Transcript(TranscriptEvent),
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    Compacting,
    Compacted {
        replaced: usize,
    },
    Warning(String),
    Usage {
        context_tokens: u64,
        context_window: u64,
    },
}

impl Update {
    pub(super) fn from_event(event: TurnEvent<'_>) -> Self {
        match event {
            TurnEvent::TextDelta(text) => {
                Self::Transcript(TranscriptEvent::TextDelta(text.to_string()))
            }
            TurnEvent::ReasoningDelta { kind, text } => {
                Self::Transcript(TranscriptEvent::ReasoningDelta {
                    kind,
                    text: text.to_string(),
                })
            }
            TurnEvent::RetryReset => Self::Transcript(TranscriptEvent::RetryReset),
            TurnEvent::Retrying {
                attempt,
                delay_ms,
                error,
            } => Self::Retrying {
                attempt,
                delay_ms,
                error,
            },
            TurnEvent::AssistantDone => Self::Transcript(TranscriptEvent::AssistantDone),
            TurnEvent::ToolStart { name, args } => Self::Transcript(TranscriptEvent::ToolStart {
                name: name.to_string(),
                args: args.to_string(),
            }),
            TurnEvent::ToolEnd {
                name,
                output,
                is_error,
            } => Self::Transcript(TranscriptEvent::ToolEnd {
                name: name.to_string(),
                output: output.to_string(),
                is_error,
            }),
            TurnEvent::Compacting => Self::Compacting,
            TurnEvent::Compacted { replaced } => Self::Compacted { replaced },
            TurnEvent::Warning(text) => Self::Warning(text),
            TurnEvent::Usage {
                context_tokens,
                context_window,
            } => Self::Usage {
                context_tokens,
                context_window,
            },
        }
    }
}

pub(super) fn advance_ticks(state: &mut ViewState) {
    state.copy_toast_ticks = state.copy_toast_ticks.saturating_sub(1);
    state.spinner_tick = state.spinner_tick.wrapping_add(1);
}
pub(super) fn scroll(state: &mut ViewState, amount: i32) {
    if amount >= 0 {
        state.scroll_offset = state.scroll_offset.saturating_add(amount as usize);
    } else {
        state.scroll_offset = state
            .scroll_offset
            .saturating_sub(amount.unsigned_abs() as usize);
    }
}

/// Thumb length and travel range for a viewport of `height` rows over
/// `total_lines` of content.
pub(super) fn scroll_bar_span(height: usize, total_lines: usize) -> (usize, usize) {
    let length = (height * height / total_lines).clamp(1, height);
    (length, height - length)
}

/// Thumb top row for the current scroll offset. `scroll_offset` counts from
/// the bottom, so an offset of zero pins the thumb to the last row.
pub(super) fn scroll_bar_position(travel: usize, max_scroll: usize, scroll_offset: usize) -> usize {
    let viewport_top = max_scroll.saturating_sub(scroll_offset);
    (viewport_top * travel / max_scroll).min(travel)
}

/// Handles presses and drags on the transcript scroll bar. Returns true when
/// the event belongs to the bar so text selection leaves it alone.
pub(super) fn handle_scroll_bar_mouse(state: &mut ViewState, event: MouseEvent) -> bool {
    match event.kind {
        MouseKind::Press => {
            let Some(geometry) = state.scroll_geometry else {
                return false;
            };
            if event.column + 1 != geometry.columns || event.row >= geometry.rows {
                state.scroll_bar_drag = None;
                return false;
            }
            let start =
                scroll_bar_position(geometry.travel, geometry.max_scroll, state.scroll_offset);
            let grab = if event.row >= start && event.row < start + geometry.thumb_length {
                event.row - start
            } else {
                geometry.thumb_length / 2
            };
            state.scroll_bar_drag = Some(grab);
            scroll_bar_jump(state, geometry, event.row, grab);
            true
        }
        MouseKind::Drag => {
            let Some(grab) = state.scroll_bar_drag else {
                return false;
            };
            if let Some(geometry) = state.scroll_geometry {
                scroll_bar_jump(state, geometry, event.row, grab);
            }
            true
        }
        MouseKind::Release => state.scroll_bar_drag.take().is_some(),
    }
}

fn scroll_bar_jump(state: &mut ViewState, geometry: ScrollGeometry, row: usize, grab: usize) {
    if geometry.travel == 0 || geometry.max_scroll == 0 {
        return;
    }
    let wanted = row.saturating_sub(grab).min(geometry.travel);
    let viewport_top = (wanted * geometry.max_scroll / geometry.travel).min(geometry.max_scroll);
    state.scroll_offset = geometry.max_scroll - viewport_top;
}
pub(super) fn toggle_tool_expansion(state: &mut ViewState) {
    state.tools_expanded = !state.tools_expanded;
    state.scroll_offset = 0;
    state.activity = if state.tools_expanded {
        "tool output expanded".into()
    } else {
        "tool output compact".into()
    };
}
