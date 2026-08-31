use crate::provider::{ImageContent, ReasoningKind, StreamNotice, TokenUsage, UsageSummary};

/// Progress events surfaced to the UI (print mode or TUI) during a turn.
pub enum TurnEvent<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
    },
    /// Discard any partial text shown so far; a retry restarts the response.
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
    /// One assistant response finished (there may be more after tools run).
    AssistantDone,
    /// Replace streamed assistant text with the goal_complete result.
    AssistantReplace(&'a str),
    /// A steering message was accepted into the current turn.
    SteerAccepted {
        text: &'a str,
    },
    /// A tool was selected but its arguments are still being generated.
    ToolPreparing {
        name: &'a str,
    },
    ToolStart {
        name: &'a str,
        args: &'a str,
    },
    ToolEnd {
        name: &'a str,
        output: &'a str,
        images: &'a [ImageContent],
        is_error: bool,
    },
    Compacting,
    Compacted {
        replaced: usize,
    },
    /// Non-fatal problem the user should know about; the turn continues.
    Warning(String),
    Usage {
        context_tokens: u64,
        context_window: u64,
        request_usage: TokenUsage,
        session_usage: UsageSummary,
    },
}

/// Adapts stream-level notices to turn events.
pub(super) fn forward<'s>(
    sink: &'s mut dyn FnMut(TurnEvent<'_>),
) -> impl FnMut(StreamNotice<'_>) + 's {
    move |notice| match notice {
        StreamNotice::TextDelta(text) => sink(TurnEvent::TextDelta(text)),
        StreamNotice::ReasoningDelta { kind, text } => {
            sink(TurnEvent::ReasoningDelta { kind, text })
        }
        StreamNotice::ToolPreparing { name } => sink(TurnEvent::ToolPreparing { name }),
        StreamNotice::RetryReset => sink(TurnEvent::RetryReset),
        StreamNotice::Retrying {
            attempt,
            delay_ms,
            error,
        } => sink(TurnEvent::Retrying {
            attempt,
            delay_ms,
            error,
        }),
    }
}
