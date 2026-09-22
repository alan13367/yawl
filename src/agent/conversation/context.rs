use super::{Conversation, ConversationKind};
use crate::error::Error;
use crate::provider::{Message, ToolSpec};
use crate::session::ContextUsage;

fn text_tokens(text: &str) -> u64 {
    // A conservative byte-based fallback, not a provider tokenizer.
    (text.len() as u64).div_ceil(3)
}

pub(super) fn message_tokens(message: &Message) -> u64 {
    message.estimated_tokens()
}

pub(super) fn prompt_tokens(system: &str, tools: &[ToolSpec]) -> u64 {
    tools.iter().fold(text_tokens(system), |tokens, tool| {
        tokens
            .saturating_add(text_tokens(&tool.name))
            .saturating_add(text_tokens(&tool.description))
            .saturating_add(text_tokens(&tool.input_schema.to_string()))
            .saturating_add(16)
    })
}

impl Conversation {
    pub(super) fn estimated_context(&self, overhead: u64) -> u64 {
        let (base, start) = match &self.context_usage {
            Some(usage)
                if self.context_tokens > 0
                    && usage.model == self.model
                    && usage.messages <= self.messages.len() =>
            {
                (
                    usage
                        .tokens
                        .saturating_sub(usage.overhead.saturating_sub(overhead))
                        .saturating_add(overhead.saturating_sub(usage.overhead)),
                    usage.messages
                        + usize::from(self.messages.get(usage.messages).is_some_and(|message| {
                            message.role == crate::provider::Role::Assistant
                        })),
                )
            }
            _ => (overhead, 0),
        };
        self.messages[start..].iter().fold(base, |tokens, message| {
            tokens.saturating_add(message_tokens(message))
        })
    }

    pub(super) fn record_context(&mut self, tokens: u64, overhead: u64) -> Result<(), Error> {
        let context = ContextUsage {
            tokens,
            // Anchor at the request history. A failed assistant append must
            // not let the next user message masquerade as measured output.
            messages: self.messages.len(),
            model: self.model.clone(),
            overhead,
        };
        if let ConversationKind::Persistent(state) = &mut self.kind {
            state.session.record_context(context.clone())?;
        }
        self.context_usage = Some(context);
        self.context_tokens = tokens;
        Ok(())
    }
}

/// Limit recovery to explicit context errors; unrelated 400/413 responses
/// should retain their original error and must not trigger summarization.
pub(super) fn is_context_limit(error: &Error) -> bool {
    let text = match error {
        Error::Http {
            status: 400 | 413 | 422,
            body,
        } => body,
        Error::Protocol(message) => message,
        _ => return false,
    }
    .to_ascii_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "prompt is too long",
        "input exceeds the context",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_does_not_double_count_subagent_report_text() {
        let message = Message::subagent_results(vec![crate::provider::SubagentResult {
            id: "a".into(),
            name: "worker".into(),
            status: "done".into(),
            run_number: 1,
            content: "large report ".repeat(1000),
        }]);
        let plain = Message::user(message.content.clone());
        assert_eq!(message_tokens(&message), message_tokens(&plain));
    }

    #[test]
    fn estimates_follow_growing_and_shrinking_prompt_overhead() {
        let mut conversation = Conversation::memory(
            crate::config::Config::test_default(),
            "test".into(),
            "child".into(),
        );
        conversation.context_tokens = 5000;
        conversation.context_usage = Some(ContextUsage {
            tokens: 5000,
            messages: 0,
            model: "test".into(),
            overhead: 2000,
        });
        assert_eq!(conversation.estimated_context(1000), 4000);
        assert_eq!(conversation.estimated_context(3000), 6000);
    }

    #[test]
    fn only_explicit_context_errors_trigger_recovery() {
        for body in [
            "context_length_exceeded",
            "maximum context length is 123",
            "prompt is too long",
        ] {
            assert!(is_context_limit(&Error::Http {
                status: 400,
                body: body.into()
            }));
        }
        for (status, body) in [
            (413, "image too large"),
            (400, "invalid tool schema"),
            (500, "context window"),
        ] {
            assert!(!is_context_limit(&Error::Http {
                status,
                body: body.into()
            }));
        }
        assert!(!is_context_limit(&Error::Interrupted));
    }
}
