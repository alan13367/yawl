use std::collections::VecDeque;

use crate::provider::{Message, ReasoningKind, Role};

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
    },
    Notice(String),
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
        }
    }

    pub(super) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn push_user(&mut self, content: String) {
        self.entries.push(Entry::User(content));
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
                    ..
                }) = index.and_then(|index| self.entries.get_mut(index))
                {
                    *entry_name = name;
                    *entry_output = output;
                    *entry_error = is_error;
                    *running = false;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Reasoning, ToolCall};

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
            }]
        );
    }
}
