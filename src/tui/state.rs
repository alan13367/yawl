//! Mutable UI state and owned agent-event updates.

use crate::agent::{Agent, TurnEvent};
use crate::config::{Config, UiColor};
use crate::subagent::{SubagentManager, SubagentSnapshot};

use super::completion::{Completion, command_completions};
use super::events::{MouseEvent, MouseKind};
use super::picker::{Picker, PickerAction};
use super::subagents::SubagentView;
use super::transcript::{Transcript, TranscriptEvent};

pub(super) const COPY_TOAST_TICKS: u8 = 15;

/// Ticks an idle transcript scroll bar stays visible before auto-hide
/// engages (~2s at the terminal's 100 ms raw-mode read timeout).
pub(super) const SCROLL_BAR_AUTO_HIDE_TICKS: u32 = 20;

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
    /// Effective selection highlight color: `selection_color` from the
    /// config when set, otherwise the accent color.
    pub(super) selection_color: UiColor,
    pub(super) show_scroll_bar: bool,
    /// Whether the config enables the scroll bar at all. Kept beside the
    /// effective `show_scroll_bar` so auto-hide can re-show it on activity.
    pub(super) scroll_bar_enabled: bool,
    pub(super) scroll_bar_auto_hide: bool,
    pub(super) scroll_bar_idle_ticks: u32,
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
    /// Slash-command or `@` mention prefix last used to rank the completion
    /// menu. When it changes, the cursor returns to the first match.
    pub(super) completion_filter: Option<String>,
    /// Lazily-built project file index behind `@` mention completion.
    pub(super) file_index: super::files::FileIndex,
    pub(super) picker: Option<Picker>,
    pub(super) connection: Option<super::connection::ConnectFlow>,
    /// Whether subagent orchestration is enabled, mirrored from the config
    /// so busy-path commands can answer without the agent.
    pub(super) subagents_enabled: bool,
    pub(super) subagent_manager: SubagentManager,
    pub(super) subagent_snapshots: Vec<SubagentSnapshot>,
    /// Session-wide usage tokens across finished subagent runs.
    pub(super) subagent_tokens: u64,
    pub(super) subagent_view: Option<SubagentView>,
    pub(super) render_cache: super::render::RenderCache,
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
            selection_color: agent.config().effective_selection_color(),
            scroll_bar_enabled: agent.config().scroll_bar,
            scroll_bar_auto_hide: agent.config().scroll_bar_auto_hide,
            show_scroll_bar: agent.config().scroll_bar,
            scroll_bar_idle_ticks: 0,
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
            completion_filter: None,
            file_index: super::files::FileIndex::default(),
            picker: None,
            connection: None,
            subagents_enabled: agent.config().subagents,
            subagent_manager: agent.subagents(),
            subagent_snapshots: agent.subagents().snapshots(),
            subagent_tokens: agent.subagents().total_child_tokens(),
            subagent_view: None,
            render_cache: super::render::RenderCache::default(),
        }
    }

    pub(super) fn refresh_completions(&mut self, agent: &Agent) {
        self.completions = command_completions(agent);
        self.completion_index = 0;
        self.completion_filter = None;
    }

    /// Mirrors scroll-bar settings from the config into runtime state and
    /// recomputes effective visibility after a settings change or reload.
    /// Unrelated settings leave an auto-hidden bar hidden.
    pub(super) fn sync_scroll_bar_config(&mut self, config: &Config) {
        let enabling = config.scroll_bar && !self.scroll_bar_enabled;
        self.scroll_bar_enabled = config.scroll_bar;
        self.scroll_bar_auto_hide = config.scroll_bar_auto_hide;
        if !config.scroll_bar {
            self.show_scroll_bar = false;
            self.scroll_bar_idle_ticks = 0;
        } else if !config.scroll_bar_auto_hide || enabling {
            self.show_scroll_bar = true;
            self.scroll_bar_idle_ticks = 0;
        }
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
            Update::ToolPreparing { name } => {
                self.activity = match name.as_str() {
                    "write_file" => "preparing write".into(),
                    "edit_file" => "preparing edit".into(),
                    _ => "preparing tool".into(),
                };
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
    ToolPreparing {
        name: String,
    },
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
            TurnEvent::ToolPreparing { name } => Self::ToolPreparing {
                name: name.to_string(),
            },
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

pub(super) fn advance_ticks(state: &mut ViewState) -> bool {
    let mut changed = false;
    if state.copy_toast_ticks > 0 {
        state.copy_toast_ticks -= 1;
        changed |= state.copy_toast_ticks == 0;
    }
    let has_active_subagent = state
        .subagent_snapshots
        .iter()
        .any(|snapshot| snapshot.status.is_active());
    let animate = super::render::loading_label(&state.activity).is_some()
        || (state.transcript.is_empty()
            && state.spinner_tick < super::render::WELCOME_ANIMATION_TICKS)
        || state.subagent_view.is_some()
        || has_active_subagent;
    if animate {
        state.spinner_tick = state.spinner_tick.wrapping_add(1);
        changed = true;
    }
    if state.scroll_bar_enabled && state.scroll_bar_auto_hide && state.scroll_bar_drag.is_none() {
        state.scroll_bar_idle_ticks = state.scroll_bar_idle_ticks.saturating_add(1);
        if state.show_scroll_bar && state.scroll_bar_idle_ticks >= SCROLL_BAR_AUTO_HIDE_TICKS {
            state.show_scroll_bar = false;
            changed = true;
        }
    }
    super::subagents::refresh(state);
    if state.transcript.poll_search() {
        changed = true;
        if state.transcript.search_position().is_some()
            && state.transcript.set_selected_expanded(true)
        {
            state.render_cache.invalidate();
        }
    }
    changed
}

/// Re-shows the scroll bar and restarts its idle timer after scrolling
/// activity. A no-op when the bar is disabled entirely.
fn wake_scroll_bar(state: &mut ViewState) {
    if !state.scroll_bar_enabled {
        return;
    }
    state.show_scroll_bar = true;
    state.scroll_bar_idle_ticks = 0;
}

pub(super) fn scroll(state: &mut ViewState, amount: i32) {
    wake_scroll_bar(state);
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
    let minimum = height.min(3);
    let length = (height * height / total_lines).clamp(minimum, height);
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
            wake_scroll_bar(state);
            true
        }
        MouseKind::Drag => {
            let Some(grab) = state.scroll_bar_drag else {
                return false;
            };
            if let Some(geometry) = state.scroll_geometry {
                scroll_bar_jump(state, geometry, event.row, grab);
                wake_scroll_bar(state);
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
    state.transcript.clear_expansion_overrides();
    state.render_cache.invalidate();
    state.scroll_offset = 0;
    state.activity = if state.tools_expanded {
        "tool output expanded".into()
    } else {
        "tool output compact".into()
    };
}
