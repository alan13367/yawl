use std::collections::VecDeque;
use std::time::Instant;

use crate::provider::{Message, ReasoningKind, Role};

use super::search::TranscriptSearch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Entry {
    User(String),
    Assistant(String),
    Reasoning {
        kind: ReasoningKind,
        content: String,
    },
    Tool {
        name: String,
        args: String,
        output: String,
        is_error: bool,
        running: bool,
        /// Live start time of a running call. Always `None` once the call
        /// settles so live and replayed transcripts stay identical.
        started: Option<Instant>,
    },
    Notice(String),
    SubagentResult {
        id: String,
        name: String,
        status: String,
        content: String,
    },
}

pub(super) enum TranscriptEvent {
    TextDelta(String),
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    RetryReset,
    AssistantDone,
    ToolStart {
        name: String,
        args: String,
    },
    ToolEnd {
        name: String,
        output: String,
        is_error: bool,
    },
}

pub(super) struct Transcript {
    entries: Vec<Entry>,
    streaming_entries_start: Option<usize>,
    streaming_assistant: Option<usize>,
    streaming_reasoning: Option<(ReasoningKind, usize)>,
    running_tool: Option<usize>,
    focused: bool,
    selected: Option<usize>,
    expansion_overrides: std::collections::BTreeMap<usize, bool>,
    viewer_open: bool,
    search: Option<TranscriptSearch>,
    reveal_selected: bool,
}

impl Transcript {
    pub(super) fn from_messages(messages: &[Message]) -> Self {
        let mut entries = Vec::new();
        let mut pending_tools = VecDeque::new();
        for message in messages {
            match message.role {
                Role::User if message.content.starts_with("[conversation summary]") => {
                    entries.push(Entry::Notice(message.content.clone()));
                }
                Role::User if !message.subagent_results.is_empty() => {
                    entries.extend(message.subagent_results.iter().map(|result| {
                        Entry::SubagentResult {
                            id: result.id.clone(),
                            name: result.name.clone(),
                            status: result.status.clone(),
                            content: result.content.clone(),
                        }
                    }));
                }
                Role::User => entries.push(Entry::User(message.content.clone())),
                Role::Assistant => {
                    for reasoning in &message.reasoning {
                        if !reasoning.content.is_empty() {
                            entries.push(Entry::Reasoning {
                                kind: reasoning.kind,
                                content: reasoning.content.clone(),
                            });
                        }
                    }
                    if !message.content.is_empty() {
                        entries.push(Entry::Assistant(message.content.clone()));
                    }
                    for call in &message.tool_calls {
                        entries.push(Entry::Tool {
                            name: call.name.clone(),
                            args: call.arguments.clone(),
                            output: String::new(),
                            is_error: false,
                            running: false,
                            started: None,
                        });
                        pending_tools.push_back((call.id.as_str(), entries.len() - 1));
                    }
                }
                Role::Tool => {
                    let pending_position = message.tool_call_id.as_deref().and_then(|id| {
                        pending_tools
                            .iter()
                            .position(|(pending_id, _)| *pending_id == id)
                    });
                    let pending_index = pending_position
                        .and_then(|position| pending_tools.remove(position))
                        .map(|(_, index)| index);
                    if let Some(Entry::Tool {
                        name,
                        output,
                        is_error,
                        ..
                    }) = pending_index.and_then(|index| entries.get_mut(index))
                    {
                        if let Some(tool_name) = &message.tool_name {
                            name.clone_from(tool_name);
                        }
                        output.clone_from(&message.content);
                        *is_error = message.is_error;
                    } else {
                        entries.push(Entry::Tool {
                            name: message.tool_name.clone().unwrap_or_else(|| "tool".into()),
                            args: String::new(),
                            output: message.content.clone(),
                            is_error: message.is_error,
                            running: false,
                            started: None,
                        });
                    }
                }
            }
        }
        Self {
            entries,
            streaming_entries_start: None,
            streaming_assistant: None,
            streaming_reasoning: None,
            running_tool: None,
            focused: false,
            selected: None,
            expansion_overrides: std::collections::BTreeMap::new(),
            viewer_open: false,
            search: None,
            reveal_selected: false,
        }
    }

    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn entry(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selected.filter(|index| *index < self.entries.len())
    }

    pub(super) fn selected_entry(&self) -> Option<&Entry> {
        self.selected_index()
            .and_then(|index| self.entries.get(index))
    }

    pub(super) fn is_focused(&self) -> bool {
        self.focused
    }

    pub(super) fn focus(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.focused = true;
        if self.selected_index().is_none() {
            self.selected = Some(self.entries.len() - 1);
        }
        self.reveal_selected = true;
    }

    pub(super) fn blur(&mut self) {
        self.focused = false;
    }

    pub(super) fn move_selection(&mut self, amount: isize) {
        if self.entries.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index().unwrap_or(self.entries.len() - 1);
        self.selected = Some(if amount < 0 {
            current.saturating_sub(amount.unsigned_abs())
        } else {
            current
                .saturating_add(amount as usize)
                .min(self.entries.len() - 1)
        });
        self.reveal_selected = true;
    }

    pub(super) fn set_selected_expanded(&mut self, expanded: bool) -> bool {
        let Some(index) = self.selected_index() else {
            return false;
        };
        self.expansion_overrides.insert(index, expanded) != Some(expanded)
    }

    pub(super) fn entry_expanded(&self, index: usize, default: bool) -> bool {
        self.expansion_overrides
            .get(&index)
            .copied()
            .unwrap_or(default)
    }

    pub(super) fn clear_expansion_overrides(&mut self) {
        self.expansion_overrides.clear();
    }

    pub(super) fn viewer_open(&self) -> bool {
        self.viewer_open
    }

    pub(super) fn open_viewer(&mut self) {
        if self.selected_entry().is_some() {
            self.viewer_open = true;
        }
    }

    pub(super) fn close_viewer(&mut self) {
        self.viewer_open = false;
    }

    pub(super) fn open_search(&mut self) {
        let corpus = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, entry.searchable_text()))
            .collect();
        self.search = Some(TranscriptSearch::open(corpus));
        self.viewer_open = false;
        self.focused = true;
    }

    pub(super) fn close_search(&mut self) {
        self.search = None;
    }

    pub(super) fn search_active(&self) -> bool {
        self.search.is_some()
    }

    pub(super) fn search_query(&self) -> Option<&str> {
        self.search.as_ref().map(TranscriptSearch::query)
    }

    pub(super) fn search_position(&self) -> Option<(usize, usize)> {
        self.search.as_ref().and_then(TranscriptSearch::position)
    }

    pub(super) fn search_push(&mut self, character: char) {
        if let Some(search) = self.search.as_mut() {
            search.push(character);
        }
    }

    pub(super) fn search_backspace(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.backspace();
        }
    }

    pub(super) fn search_paste(&mut self, text: &str) {
        if let Some(search) = self.search.as_mut() {
            search.paste(text);
        }
    }

    pub(super) fn search_next(&mut self, reverse: bool) {
        if let Some(index) = self.search.as_mut().and_then(|search| search.next(reverse)) {
            self.selected = Some(index);
            self.reveal_selected = true;
        }
    }

    pub(super) fn poll_search(&mut self) -> bool {
        let Some(result) = self.search.as_mut().and_then(TranscriptSearch::poll) else {
            return false;
        };
        if let Some(index) = result {
            self.selected = Some(index);
            self.reveal_selected = true;
        }
        true
    }

    pub(super) fn take_reveal_selected(&mut self) -> bool {
        std::mem::take(&mut self.reveal_selected)
    }

    pub(super) fn last_assistant_text(&self) -> Option<&str> {
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Assistant(text) if !text.is_empty() => Some(text.as_str()),
            _ => None,
        })
    }

    pub(super) fn has_streaming_assistant(&self) -> bool {
        self.streaming_assistant.is_some()
    }

    pub(super) fn streaming_index(&self) -> Option<usize> {
        self.streaming_assistant
            .or_else(|| self.streaming_reasoning.map(|(_, index)| index))
    }

    pub(super) fn running_tool_index(&self) -> Option<usize> {
        self.running_tool
    }

    pub(super) fn push_user(&mut self, content: String) {
        self.entries.push(Entry::User(content));
        if self.focused {
            self.selected = Some(self.entries.len() - 1);
        }
    }

    pub(super) fn notice(&mut self, text: String) {
        self.entries.push(Entry::Notice(text));
    }

    pub(super) fn apply(&mut self, event: TranscriptEvent) {
        match event {
            TranscriptEvent::TextDelta(text) => {
                self.streaming_entries_start
                    .get_or_insert(self.entries.len());
                self.streaming_reasoning = None;
                let index = match self.streaming_assistant {
                    Some(index) => index,
                    None => {
                        self.entries.push(Entry::Assistant(String::new()));
                        let index = self.entries.len() - 1;
                        self.streaming_assistant = Some(index);
                        index
                    }
                };
                if let Some(Entry::Assistant(content)) = self.entries.get_mut(index) {
                    content.push_str(&text);
                }
            }
            TranscriptEvent::ReasoningDelta { kind, text } => {
                self.streaming_entries_start
                    .get_or_insert(self.entries.len());
                self.streaming_assistant = None;
                let index = match self.streaming_reasoning {
                    Some((current_kind, index)) if current_kind == kind => index,
                    _ => {
                        self.entries.push(Entry::Reasoning {
                            kind,
                            content: String::new(),
                        });
                        let index = self.entries.len() - 1;
                        self.streaming_reasoning = Some((kind, index));
                        index
                    }
                };
                if let Some(Entry::Reasoning { content, .. }) = self.entries.get_mut(index) {
                    content.push_str(&text);
                }
            }
            TranscriptEvent::RetryReset => {
                if let Some(start) = self.streaming_entries_start {
                    self.entries.truncate(start);
                }
                self.streaming_assistant = None;
                self.streaming_reasoning = None;
                self.selected = self.selected.filter(|index| *index < self.entries.len());
                self.expansion_overrides
                    .retain(|index, _| *index < self.entries.len());
            }
            TranscriptEvent::AssistantDone => {
                self.streaming_entries_start = None;
                self.streaming_assistant = None;
                self.streaming_reasoning = None;
            }
            TranscriptEvent::ToolStart { name, args } => {
                self.entries.push(Entry::Tool {
                    name,
                    args,
                    output: String::new(),
                    is_error: false,
                    running: true,
                    started: Some(Instant::now()),
                });
                self.running_tool = Some(self.entries.len() - 1);
            }
            TranscriptEvent::ToolEnd {
                name,
                output,
                is_error,
            } => {
                let index = self.running_tool.take();
                if let Some(Entry::Tool {
                    name: entry_name,
                    output: entry_output,
                    is_error: entry_error,
                    running,
                    started,
                    ..
                }) = index.and_then(|index| self.entries.get_mut(index))
                {
                    *entry_name = name;
                    *entry_output = output;
                    *entry_error = is_error;
                    *running = false;
                    *started = None;
                }
            }
        }
    }
}

impl Entry {
    pub(super) fn searchable_text(&self) -> String {
        match self {
            Self::User(text)
            | Self::Assistant(text)
            | Self::Notice(text)
            | Self::Reasoning { content: text, .. } => text.clone(),
            Self::Tool {
                name, args, output, ..
            } => format!("{name}\n{args}\n{output}"),
            Self::SubagentResult {
                id,
                name,
                status,
                content,
            } => format!("{id}\n{name}\n{status}\n{content}"),
        }
    }

    pub(super) fn copy_text(&self) -> String {
        match self {
            Self::Tool { output, args, .. } if output.is_empty() => args.clone(),
            Self::Tool { output, .. } => output.clone(),
            Self::User(text)
            | Self::Assistant(text)
            | Self::Notice(text)
            | Self::Reasoning { content: text, .. } => text.clone(),
            Self::SubagentResult { content, .. } => content.clone(),
        }
    }

    pub(super) fn label(&self) -> &str {
        match self {
            Self::User(_) => "Prompt",
            Self::Assistant(_) => "Reply",
            Self::Reasoning { .. } => "Reasoning",
            Self::Tool { name, .. } => name,
            Self::Notice(_) => "Notice",
            Self::SubagentResult { name, .. } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Reasoning, SubagentResult, ToolCall};

    #[test]
    fn live_events_and_replayed_messages_produce_the_same_entries() {
        let mut assistant = Message::assistant(
            "hello".into(),
            vec![ToolCall {
                id: "id".into(),
                name: "shell".into(),
                arguments: r#"{"command":"pwd"}"#.into(),
            }],
        );
        assistant.reasoning.push(Reasoning {
            kind: ReasoningKind::Summary,
            content: "Checking the directory".into(),
        });
        let replayed = Transcript::from_messages(&[
            Message::user("hi"),
            assistant,
            Message::tool_result("id", "shell", "ok".into(), false),
        ]);

        let mut live = Transcript::from_messages(&[]);
        live.push_user("hi".into());
        live.apply(TranscriptEvent::ReasoningDelta {
            kind: ReasoningKind::Summary,
            text: "Checking the directory".into(),
        });
        live.apply(TranscriptEvent::TextDelta("hello".into()));
        live.apply(TranscriptEvent::AssistantDone);
        live.apply(TranscriptEvent::ToolStart {
            name: "shell".into(),
            args: r#"{"command":"pwd"}"#.into(),
        });
        live.apply(TranscriptEvent::ToolEnd {
            name: "shell".into(),
            output: "ok".into(),
            is_error: false,
        });

        assert_eq!(live.entries(), replayed.entries());
    }

    #[test]
    fn retry_reset_discards_only_the_partial_assistant_response() {
        let mut transcript = Transcript::from_messages(&[Message::user("hi")]);
        transcript.apply(TranscriptEvent::ReasoningDelta {
            kind: ReasoningKind::Summary,
            text: "partial reasoning".into(),
        });
        transcript.apply(TranscriptEvent::TextDelta("partial answer".into()));
        transcript.apply(TranscriptEvent::RetryReset);
        transcript.apply(TranscriptEvent::TextDelta("complete answer".into()));
        transcript.apply(TranscriptEvent::AssistantDone);

        assert_eq!(
            transcript.entries(),
            &[
                Entry::User("hi".into()),
                Entry::Assistant("complete answer".into()),
            ]
        );
    }

    #[test]
    fn replay_keeps_a_tool_call_without_a_result() {
        let transcript = Transcript::from_messages(&[Message::assistant(
            String::new(),
            vec![ToolCall {
                id: "id".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        )]);

        assert_eq!(
            transcript.entries(),
            &[Entry::Tool {
                name: "shell".into(),
                args: "{}".into(),
                output: String::new(),
                is_error: false,
                running: false,
                started: None,
            }]
        );
    }

    #[test]
    fn replayed_subagent_results_use_dedicated_entries() {
        let message = Message::subagent_results(vec![SubagentResult {
            id: "sa-1".into(),
            name: "review".into(),
            status: "completed".into(),
            run_number: 1,
            content: "done".into(),
        }]);
        let transcript = Transcript::from_messages(&[message]);

        assert_eq!(
            transcript.entries(),
            &[Entry::SubagentResult {
                id: "sa-1".into(),
                name: "review".into(),
                status: "completed".into(),
                content: "done".into(),
            }]
        );
    }

    #[test]
    fn focus_does_nothing_when_the_transcript_is_empty() {
        let mut transcript = Transcript::from_messages(&[]);
        transcript.focus();
        assert!(!transcript.is_focused());
        assert_eq!(transcript.selected_index(), None);
    }

    #[test]
    fn search_selects_the_matching_entry_for_reveal() {
        let mut transcript = Transcript::from_messages(&[
            Message::assistant("alpha".into(), vec![]),
            Message::assistant("beta unique".into(), vec![]),
        ]);
        transcript.open_search();
        for character in "unique".chars() {
            transcript.search_push(character);
        }
        for _ in 0..100 {
            if transcript.poll_search() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(transcript.selected_index(), Some(1));
        assert!(transcript.take_reveal_selected());
        assert!(transcript.set_selected_expanded(true));
    }
}
