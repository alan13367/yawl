use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::TurnEvent;
use crate::provider::{ReasoningKind, UsageSummary};

pub(crate) const MAX_TRACKED_SUBAGENTS: usize = 64;
pub(crate) const MAX_QUEUE_MESSAGES: usize = 16;
pub(crate) const MAX_PROMPT_CHARS: usize = 60_000;
pub(crate) const MAX_ERROR_BYTES: usize = 4 * 1024;
pub(crate) const MAX_TRANSCRIPT_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LIVE_TEXT_BYTES: usize = 128 * 1024;
pub(crate) const MAX_FINAL_RESULT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TRANSCRIPT_ITEMS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SubagentId(String);

impl SubagentId {
    pub(crate) fn new(sequence: u64) -> Self {
        Self(format!("sa-{sequence}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SubagentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubagentStatus {
    Starting,
    Running,
    Canceling,
    Done,
    Failed,
}

impl SubagentStatus {
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Canceling)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Canceling => "canceling",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOrigin {
    Model,
    PrivateUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunOutcome {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubagentTranscriptItem {
    User {
        text: String,
        private: bool,
    },
    Steer(String),
    Assistant(String),
    Reasoning {
        kind: ReasoningKind,
        text: String,
    },
    Tool {
        name: String,
        arguments: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedSubagentMessage {
    pub(crate) text: String,
    pub(crate) origin: RunOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveTool {
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) output: String,
    pub(crate) is_error: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentSnapshot {
    pub(crate) id: SubagentId,
    pub(crate) name: String,
    /// Preset name ("default" for the unrestricted child).
    pub(crate) agent: String,
    pub(crate) initial_prompt: String,
    pub(crate) model: String,
    pub(crate) origin: RunOrigin,
    pub(crate) status: SubagentStatus,
    pub(crate) created_at: Instant,
    pub(crate) started_at: Option<Instant>,
    pub(crate) settled_at: Option<Instant>,
    pub(crate) context_tokens: u64,
    pub(crate) context_window: u64,
    /// Completed model requests in the current run.
    pub(crate) requests: u64,
    /// Summed usage tokens across the requests of the current run.
    pub(crate) run_tokens: u64,
    pub(crate) run_usage: UsageSummary,
    /// Usage across the retained lifetime of this child conversation.
    pub(crate) usage: UsageSummary,
    pub(crate) transcript: Vec<Arc<SubagentTranscriptItem>>,
    pub(crate) live_assistant: String,
    pub(crate) live_reasoning: String,
    pub(crate) live_reasoning_kind: Option<ReasoningKind>,
    pub(crate) current_tool: Option<LiveTool>,
    pub(crate) queued_messages: Vec<QueuedSubagentMessage>,
    pub(crate) pending_steers: Vec<String>,
    pub(crate) latest_final_result: String,
    pub(crate) error: String,
    pub(crate) completed_turns: u64,
    pub(crate) current_activity: String,
    pub(crate) latest_outcome: Option<RunOutcome>,
    pub(crate) run_number: u64,
}

impl SubagentSnapshot {
    pub(crate) fn new(
        id: SubagentId,
        name: String,
        agent: String,
        prompt: String,
        model: String,
        context_window: u64,
    ) -> Self {
        Self {
            id,
            name,
            agent,
            initial_prompt: prompt,
            model,
            origin: RunOrigin::Model,
            status: SubagentStatus::Starting,
            created_at: Instant::now(),
            started_at: None,
            settled_at: None,
            context_tokens: 0,
            context_window,
            requests: 0,
            run_tokens: 0,
            run_usage: UsageSummary::default(),
            usage: UsageSummary::default(),
            transcript: Vec::new(),
            live_assistant: String::new(),
            live_reasoning: String::new(),
            live_reasoning_kind: None,
            current_tool: None,
            queued_messages: Vec::new(),
            pending_steers: Vec::new(),
            latest_final_result: String::new(),
            error: String::new(),
            completed_turns: 0,
            current_activity: "starting".into(),
            latest_outcome: None,
            run_number: 1,
        }
    }

    /// UI summaries omit conversation text. Only the selected takeover receives
    /// shared immutable entries and the current live tail.
    pub(crate) fn display_snapshot(&self, detail: bool) -> Self {
        let mut result = Self::new(
            self.id.clone(),
            self.name.clone(),
            self.agent.clone(),
            String::new(),
            self.model.clone(),
            self.context_window,
        );
        result.status = self.status;
        result.created_at = self.created_at;
        result.started_at = self.started_at;
        result.settled_at = self.settled_at;
        result.context_tokens = self.context_tokens;
        result.queued_messages = self
            .queued_messages
            .iter()
            .map(|message| QueuedSubagentMessage {
                text: if detail {
                    message.text.clone()
                } else {
                    String::new()
                },
                origin: message.origin,
            })
            .collect();
        if detail {
            result.transcript.clone_from(&self.transcript);
            result.live_assistant.clone_from(&self.live_assistant);
            result.live_reasoning.clone_from(&self.live_reasoning);
            result.live_reasoning_kind = self.live_reasoning_kind;
            result.current_tool.clone_from(&self.current_tool);
            result.pending_steers.clone_from(&self.pending_steers);
            result.error.clone_from(&self.error);
        }
        result
    }

    pub(crate) fn elapsed(&self, now: Instant) -> Duration {
        let start = self.started_at.unwrap_or(self.created_at);
        self.settled_at
            .unwrap_or(now)
            .saturating_duration_since(start)
    }

    pub(crate) fn begin_turn(&mut self, text: &str, origin: RunOrigin, run_number: u64) {
        self.origin = origin;
        self.run_number = run_number;
        self.status = SubagentStatus::Running;
        self.started_at.get_or_insert_with(Instant::now);
        self.settled_at = None;
        self.error.clear();
        self.latest_outcome = None;
        self.requests = 0;
        self.run_tokens = 0;
        self.run_usage = UsageSummary::default();
        self.current_activity = "sending".into();
        self.push_transcript(SubagentTranscriptItem::User {
            text: bounded(text, MAX_TRANSCRIPT_TEXT_BYTES),
            private: origin == RunOrigin::PrivateUser,
        });
    }

    pub(crate) fn apply_event(&mut self, event: TurnEvent<'_>) {
        match event {
            TurnEvent::TextDelta(text) => {
                self.current_activity = "responding".into();
                append_bounded(&mut self.live_assistant, text, MAX_LIVE_TEXT_BYTES);
            }
            TurnEvent::ReasoningDelta { kind, text } => {
                self.current_activity = "reasoning".into();
                if self
                    .live_reasoning_kind
                    .is_some_and(|current| current != kind)
                    && !self.live_reasoning.is_empty()
                {
                    self.finalize_reasoning();
                }
                self.live_reasoning_kind = Some(kind);
                append_bounded(&mut self.live_reasoning, text, MAX_LIVE_TEXT_BYTES);
            }
            TurnEvent::RetryReset => {
                self.live_assistant.clear();
                self.live_reasoning.clear();
                self.live_reasoning_kind = None;
                self.current_tool = None;
                self.current_activity = "retrying".into();
            }
            TurnEvent::Retrying { attempt, .. } => {
                self.current_activity = format!("retrying attempt {attempt}");
            }
            TurnEvent::AssistantDone => {
                self.finalize_reasoning();
                if !self.live_assistant.is_empty() {
                    let text = std::mem::take(&mut self.live_assistant);
                    self.push_transcript(SubagentTranscriptItem::Assistant(text));
                }
                self.current_activity.clear();
            }
            TurnEvent::AssistantReplace(text) => {
                self.live_assistant = bounded(text, MAX_LIVE_TEXT_BYTES);
            }
            TurnEvent::SteerAccepted { text } => {
                if !self.pending_steers.is_empty() {
                    self.pending_steers.remove(0);
                }
                self.push_transcript(SubagentTranscriptItem::Steer(text.to_string()));
            }
            TurnEvent::ToolPreparing { name } => {
                self.current_activity = match name {
                    "write_file" => "preparing write".into(),
                    "edit_file" => "preparing edit".into(),
                    "read_skill" => "loading skill".into(),
                    _ => "preparing tool".into(),
                };
            }
            TurnEvent::ToolStart { name, args } => {
                self.current_activity = if name == "read_skill" {
                    "loading skill".into()
                } else {
                    format!("running {name}")
                };
                self.current_tool = Some(LiveTool {
                    name: sanitize_preview(name, MAX_TRANSCRIPT_TEXT_BYTES),
                    arguments: sanitize_preview(args, MAX_TRANSCRIPT_TEXT_BYTES),
                    output: String::new(),
                    is_error: false,
                });
            }
            TurnEvent::ToolEnd {
                name,
                output,
                images: _,
                is_error,
            } => {
                let arguments = self
                    .current_tool
                    .take()
                    .map_or_else(String::new, |tool| tool.arguments);
                self.push_transcript(SubagentTranscriptItem::Tool {
                    name: sanitize_preview(name, MAX_TRANSCRIPT_TEXT_BYTES),
                    arguments,
                    output: sanitize_preview(output, MAX_TRANSCRIPT_TEXT_BYTES),
                    is_error,
                });
                self.current_activity = "sending".into();
            }
            TurnEvent::Compacting => self.current_activity = "compacting".into(),
            TurnEvent::Compacted { .. } => {
                self.run_usage.record_cache_reset();
                self.current_activity = "sending".into();
            }
            TurnEvent::Warning(text) => self.error = bounded(&text, MAX_ERROR_BYTES),
            TurnEvent::Usage {
                context_tokens,
                context_window,
                request_usage,
                session_usage,
            } => {
                self.context_tokens = context_tokens;
                self.context_window = context_window;
                self.run_usage.record(request_usage);
                self.requests = self.run_usage.requests;
                self.run_tokens = self.run_usage.tokens.total_tokens();
                self.usage = session_usage;
            }
        }
    }

    pub(crate) fn finish_turn(&mut self, outcome: RunOutcome, result: &str, error: Option<&str>) {
        self.finalize_reasoning();
        if !self.live_assistant.is_empty() {
            let text = std::mem::take(&mut self.live_assistant);
            self.push_transcript(SubagentTranscriptItem::Assistant(text));
        }
        self.current_tool = None;
        self.latest_outcome = Some(outcome);
        self.latest_final_result = bounded(result, MAX_FINAL_RESULT_BYTES);
        self.error = error.map_or_else(String::new, |text| bounded(text, MAX_ERROR_BYTES));
        self.completed_turns = self.completed_turns.saturating_add(1);
        self.current_activity.clear();
    }

    fn finalize_reasoning(&mut self) {
        let Some(kind) = self.live_reasoning_kind.take() else {
            return;
        };
        if self.live_reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.live_reasoning);
        self.push_transcript(SubagentTranscriptItem::Reasoning { kind, text });
    }

    pub(super) fn push_transcript(&mut self, mut item: SubagentTranscriptItem) {
        match &mut item {
            SubagentTranscriptItem::User { text, .. }
            | SubagentTranscriptItem::Steer(text)
            | SubagentTranscriptItem::Assistant(text)
            | SubagentTranscriptItem::Reasoning { text, .. } => {
                *text = bounded(text, MAX_TRANSCRIPT_TEXT_BYTES);
            }
            SubagentTranscriptItem::Tool {
                name,
                arguments,
                output,
                ..
            } => {
                *name = bounded(name, MAX_TRANSCRIPT_TEXT_BYTES);
                *arguments = bounded(arguments, MAX_TRANSCRIPT_TEXT_BYTES);
                *output = bounded(output, MAX_TRANSCRIPT_TEXT_BYTES);
            }
        }
        if self.transcript.len() == MAX_TRANSCRIPT_ITEMS {
            self.transcript.remove(0);
        }
        self.transcript.push(Arc::new(item));
    }
}

pub(crate) fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text[..end].to_string()
}

fn append_bounded(target: &mut String, addition: &str, limit: usize) {
    if target.len() >= limit {
        return;
    }
    let remaining = limit - target.len();
    target.push_str(&bounded(addition, remaining));
}

pub(crate) fn sanitize_preview(text: &str, limit: usize) -> String {
    let mut output = String::with_capacity(text.len().min(limit));
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.next_if_eq(&'[').is_some() {
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if character.is_control() {
            if !output.ends_with(' ') {
                output.push(' ');
            }
        } else {
            output.push(character);
        }
        if output.len() >= limit {
            break;
        }
    }
    bounded(output.trim(), limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> SubagentSnapshot {
        SubagentSnapshot::new(
            SubagentId::new(1),
            "test".into(),
            "default".into(),
            "prompt".into(),
            "model".into(),
            100,
        )
    }

    #[test]
    fn display_summaries_omit_text_and_detail_shares_completed_entries() {
        let mut source = snapshot();
        source.apply_event(TurnEvent::TextDelta("completed"));
        source.apply_event(TurnEvent::AssistantDone);
        source.apply_event(TurnEvent::TextDelta("live"));
        source.latest_final_result = "large final result".repeat(1000);
        source.queued_messages.push(QueuedSubagentMessage {
            text: "private next turn".into(),
            origin: RunOrigin::PrivateUser,
        });
        let summary = source.display_snapshot(false);
        assert_eq!(summary.status, source.status);
        assert!(summary.transcript.is_empty());
        assert!(summary.live_assistant.is_empty());
        assert!(summary.initial_prompt.is_empty());
        assert!(summary.latest_final_result.is_empty());
        assert_eq!(summary.queued_messages.len(), 1);
        assert!(summary.queued_messages[0].text.is_empty());
        let detail = source.display_snapshot(true);
        assert!(Arc::ptr_eq(&detail.transcript[0], &source.transcript[0]));
        assert_eq!(detail.live_assistant, "live");
        assert_eq!(detail.queued_messages[0].text, "private next turn");
        source.apply_event(TurnEvent::TextDelta(" more"));
        assert_eq!(
            detail.live_assistant, "live",
            "live tails remain immutable snapshots"
        );
    }

    #[test]
    fn retry_discards_only_live_buffers() {
        let mut snapshot = snapshot();
        snapshot.apply_event(TurnEvent::TextDelta("partial"));
        snapshot.apply_event(TurnEvent::RetryReset);
        snapshot.apply_event(TurnEvent::TextDelta("complete"));
        snapshot.apply_event(TurnEvent::AssistantDone);

        assert_eq!(
            snapshot.transcript,
            [Arc::new(SubagentTranscriptItem::Assistant(
                "complete".into()
            ))]
        );
    }

    #[test]
    fn previews_strip_terminal_controls_and_newlines() {
        assert_eq!(sanitize_preview("one\n\u{1b}[31mtwo\u{7}", 100), "one two");
    }

    #[test]
    fn live_buffers_and_transcript_are_bounded() {
        let mut snapshot = snapshot();
        let text = "x".repeat(MAX_LIVE_TEXT_BYTES + 20);
        snapshot.apply_event(TurnEvent::TextDelta(&text));
        assert_eq!(snapshot.live_assistant.len(), MAX_LIVE_TEXT_BYTES);
        snapshot.apply_event(TurnEvent::AssistantDone);
        assert!(matches!(
            snapshot.transcript.last().map(Arc::as_ref),
            Some(SubagentTranscriptItem::Assistant(text))
                if text.len() == MAX_TRANSCRIPT_TEXT_BYTES
        ));
        for _ in 0..MAX_TRANSCRIPT_ITEMS + 10 {
            snapshot.apply_event(TurnEvent::AssistantDone);
            snapshot.apply_event(TurnEvent::TextDelta("x"));
        }
        snapshot.apply_event(TurnEvent::AssistantDone);
        assert_eq!(snapshot.transcript.len(), MAX_TRANSCRIPT_ITEMS);
    }

    #[test]
    fn snapshot_reports_file_tool_preparation() {
        let mut snapshot = snapshot();

        snapshot.apply_event(TurnEvent::ToolPreparing { name: "write_file" });
        assert_eq!(snapshot.current_activity, "preparing write");

        snapshot.apply_event(TurnEvent::ToolPreparing { name: "edit_file" });
        assert_eq!(snapshot.current_activity, "preparing edit");
    }

    #[test]
    fn snapshot_folds_reasoning_tools_usage_and_settlement() {
        let mut snapshot = snapshot();
        snapshot.begin_turn("inspect", RunOrigin::Model, 2);
        snapshot.apply_event(TurnEvent::ReasoningDelta {
            kind: ReasoningKind::Summary,
            text: "checking",
        });
        snapshot.apply_event(TurnEvent::TextDelta("answer"));
        snapshot.apply_event(TurnEvent::AssistantDone);
        snapshot.apply_event(TurnEvent::ToolStart {
            name: "shell",
            args: "echo\n\u{1b}[31mred",
        });
        snapshot.apply_event(TurnEvent::ToolEnd {
            name: "shell",
            output: "ok\nnext",
            images: &[],
            is_error: false,
        });
        snapshot.apply_event(TurnEvent::Usage {
            context_tokens: 40,
            context_window: 100,
            request_usage: crate::provider::TokenUsage {
                input_tokens: 32,
                output_tokens: 8,
                cached_input_tokens: 24,
                cache_write_input_tokens: 0,
                cache_details_reported: true,
            },
            session_usage: crate::provider::UsageSummary {
                requests: 1,
                tokens: crate::provider::TokenUsage {
                    input_tokens: 32,
                    output_tokens: 8,
                    cached_input_tokens: 24,
                    cache_write_input_tokens: 0,
                    cache_details_reported: true,
                },
                cache_reported_input_tokens: 32,
                cache_resets: 0,
            },
        });
        snapshot.finish_turn(RunOutcome::Completed, "done", None);

        assert_eq!(snapshot.run_number, 2);
        assert_eq!(snapshot.context_tokens, 40);
        assert_eq!(snapshot.usage.cache_hit_percent(), 75);
        assert_eq!(snapshot.latest_outcome, Some(RunOutcome::Completed));
        assert_eq!(snapshot.latest_final_result, "done");
        assert!(snapshot.transcript.iter().any(|item| {
            matches!(item.as_ref(), SubagentTranscriptItem::Reasoning { text, .. } if text == "checking")
        }));
        assert!(snapshot.transcript.iter().any(|item| {
            matches!(item.as_ref(), SubagentTranscriptItem::Tool { arguments, output, .. }
                if arguments == "echo red" && output == "ok next")
        }));
    }
}
