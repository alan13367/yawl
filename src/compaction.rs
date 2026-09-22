//! Auto-compaction: when the context is ~85% full (estimated from
//! provider-reported usage), the same model summarizes older messages while
//! retaining up to ~10 recent messages within an estimated token budget. The latest undoable user prompt is also
//! retained so `/undo` keeps its checkpoint anchor. The session JSONL keeps
//! the full original history; compaction is recorded as an event.

use std::fmt::Write as _;
use std::ops::Range;

use crate::error::Error;
use crate::provider::{Message, Provider, Request, Role, StreamNotice, TokenUsage, stream_turn};

/// Maximum ordinary tail length before applying the estimated token budget.
pub const KEEP_TAIL: usize = 10;
const TAIL_TOKEN_BUDGET: u64 = 8_000;

const SUMMARY_MARKER: &str = "[conversation summary]";

pub fn summary_message(summary: &str) -> Message {
    summary_message_with_provider_data(summary, Vec::new(), None)
}

pub(crate) fn summary_message_with_provider_data(
    summary: &str,
    provider_data: Vec<serde_json::Value>,
    provider_data_model: Option<String>,
) -> Message {
    let mut message = Message::user(format!(
        "{SUMMARY_MARKER}\nEarlier conversation, summarized to free context:\n\n{summary}"
    ));
    message.provider_data = provider_data;
    message.provider_data_model = provider_data_model;
    message
}

pub(crate) fn is_summary_message(message: &Message) -> bool {
    message.role == Role::User && message.content.starts_with(SUMMARY_MARKER)
}

/// True once the last known context usage crosses the threshold.
pub fn should_compact(context_tokens: u64, context_window: u64, threshold: f64) -> bool {
    context_tokens > 0 && (context_tokens as f64) >= (context_window as f64) * threshold
}

/// Picks the split index: everything before it is summarized, the rest is
/// kept. Walks the boundary back so a tool result is never separated from
/// the assistant message carrying its tool call.
pub fn split_point(messages: &[Message]) -> usize {
    let mut split = messages.len().saturating_sub(KEEP_TAIL);
    while split > 0 && messages[split].role == Role::Tool {
        split -= 1;
    }
    let mut tail_tokens = messages[split..]
        .iter()
        .map(Message::estimated_tokens)
        .sum::<u64>();
    while tail_tokens > TAIL_TOKEN_BUDGET {
        let mut next = split.saturating_add(1);
        while next < messages.len() && messages[next].role == Role::Tool {
            next += 1;
        }
        // Always retain the latest complete exchange, even if it is oversized.
        if next >= messages.len() {
            break;
        }
        tail_tokens = tail_tokens.saturating_sub(
            messages[split..next]
                .iter()
                .map(Message::estimated_tokens)
                .sum(),
        );
        split = next;
    }
    split
}

/// Selects a contiguous range to summarize without removing `protected`.
/// Prefer the side with more estimated tokens so compaction still frees
/// useful space during a long-running turn.
pub(crate) fn compaction_range(messages: &[Message], protected: Option<usize>) -> Range<usize> {
    let split = split_point(messages);
    let Some(protected) = protected.filter(|index| *index < split) else {
        return 0..split;
    };
    let before = 0..protected;
    let after = protected.saturating_add(1)..split;
    if !after.is_empty()
        && messages[after.clone()]
            .iter()
            .map(Message::estimated_tokens)
            .sum::<u64>()
            >= messages[before.clone()]
                .iter()
                .map(Message::estimated_tokens)
                .sum::<u64>()
    {
        after
    } else {
        before
    }
}

/// Renders messages as a plain transcript for the summarizer.
pub(crate) fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m.role {
            Role::User => {
                if m.is_hidden_control() {
                    continue;
                }
                out.push_str("## user\n");
                out.push_str(&m.content);
            }
            Role::Assistant => {
                out.push_str("## assistant\n");
                out.push_str(&m.content);
                for tc in &m.tool_calls {
                    write!(
                        out,
                        "\n[called tool {} with {}]",
                        tc.name,
                        if tc.name == crate::tools::USER_INPUT_TOOL_NAME {
                            tc.arguments.clone()
                        } else {
                            crate::error::truncate(&tc.arguments, 400)
                        }
                    )
                    .expect("writing to a String cannot fail");
                }
            }
            Role::Tool => {
                writeln!(
                    out,
                    "## tool result ({}{})",
                    m.tool_name.as_deref().unwrap_or("?"),
                    if m.is_error { ", error" } else { "" }
                )
                .expect("writing to a String cannot fail");
                if m.tool_name.as_deref() == Some(crate::tools::USER_INPUT_TOOL_NAME) {
                    out.push_str(&m.content);
                } else {
                    out.push_str(&crate::error::truncate(&m.content, 2_000));
                }
            }
        }
        out.push_str("\n\n");
    }
    out
}

const SUMMARIZER_SYSTEM: &str = "You are a conversation compactor. Summarize the transcript you \
are given so an AI agent can seamlessly continue the session. Preserve: the user's goals and \
constraints, decisions made and why, exact file paths, code identifiers, commands that were run \
and their key outcomes, unresolved problems, and what was about to happen next. Be dense and \
factual; use bullet points; do not add commentary.";

/// Runs one compaction: asks `provider`/`model` for a summary of
/// `messages[..split]` and splices it in as the new head. Returns
/// `(summary, replaced_count)` so the caller can log the session event.
pub fn compact(
    provider: &dyn Provider,
    model: &str,
    max_tokens: u32,
    messages: &mut Vec<Message>,
    sink: &mut dyn FnMut(StreamNotice<'_>),
) -> Result<(String, usize), Error> {
    let (summary, range, _) = summarize(provider, model, max_tokens, messages, None, sink)?;
    debug_assert_eq!(range.start, 0);
    let split = range.end;
    apply_summary(messages, &summary, split);
    Ok((summary, split))
}

/// Produces a compaction summary without changing the conversation. The
/// caller can persist the summary before applying it in memory.
pub(crate) fn summarize(
    provider: &dyn Provider,
    model: &str,
    max_tokens: u32,
    messages: &[Message],
    protected: Option<usize>,
    sink: &mut dyn FnMut(StreamNotice<'_>),
) -> Result<(String, Range<usize>, TokenUsage), Error> {
    let range = compaction_range(messages, protected);
    if range.is_empty() {
        return Err(Error::Config(
            "nothing to compact: conversation is too short".into(),
        ));
    }
    let ask = Message::user(format!(
        "Summarize this transcript per your instructions:\n\n{}",
        transcript(&messages[range.clone()])
    ));
    let request = Request {
        model,
        system: SUMMARIZER_SYSTEM,
        messages: std::slice::from_ref(&ask),
        tools: &[],
        max_tokens,
        supports_images: false,
        // Do not add cache controls or routing hints for this one-off request.
        // Providers with implicit caching may still cache it.
        prompt_cache_control: false,
        prompt_cache_key: None,
    };
    let out = stream_turn(provider, &request, sink)?;
    if out.text.trim().is_empty() {
        return Err(Error::Protocol("summarizer returned empty text".into()));
    }
    let summary = out.text.trim().to_string();
    Ok((summary, range, out.usage))
}

pub(crate) fn apply_summary(messages: &mut Vec<Message>, summary: &str, split: usize) {
    apply_summary_range(messages, summary, 0..split);
}

pub(crate) fn apply_summary_range(messages: &mut Vec<Message>, summary: &str, range: Range<usize>) {
    apply_summary_range_with_provider_data(messages, summary, range, Vec::new(), None);
}

pub(crate) fn apply_summary_range_with_provider_data(
    messages: &mut Vec<Message>,
    summary: &str,
    range: Range<usize>,
    provider_data: Vec<serde_json::Value>,
    provider_data_model: Option<String>,
) {
    drop(messages.splice(
        range,
        std::iter::once(summary_message_with_provider_data(
            summary,
            provider_data,
            provider_data_model,
        )),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Event, ToolCall};

    #[test]
    fn questions_and_user_answers_are_never_truncated_for_summaries() {
        let arguments = format!("{} final question", "q".repeat(3000));
        let answer = format!("{} essential constraint", "answer ".repeat(1000));
        let messages = vec![
            Message::assistant(
                String::new(),
                vec![ToolCall {
                    id: "q".into(),
                    name: crate::tools::USER_INPUT_TOOL_NAME.into(),
                    arguments: arguments.clone(),
                }],
            ),
            Message::tool_result(
                "q",
                crate::tools::USER_INPUT_TOOL_NAME,
                answer.clone(),
                false,
            ),
        ];
        let rendered = transcript(&messages);
        assert!(rendered.contains(&arguments));
        assert!(rendered.contains(&answer));
    }

    #[test]
    fn large_recent_results_compact_even_with_fewer_than_ten_messages() {
        let messages = vec![
            Message::user("Keep this undo anchor"),
            Message::assistant(
                String::new(),
                vec![ToolCall {
                    id: "big".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("big", "shell", "x".repeat(60_000), false),
            Message::assistant(
                String::new(),
                vec![ToolCall {
                    id: "small".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("small", "shell", "done".into(), false),
        ];
        assert_eq!(compaction_range(&messages, Some(0)), 1..3);
        assert_eq!(split_point(&messages), 3);
        assert!(crate::session::missing_tool_results(&messages[3..]).is_none());
    }

    #[test]
    fn compaction_prefers_the_larger_token_range_over_more_messages() {
        let mut messages = vec![Message::assistant("x".repeat(60_000), vec![])];
        messages.push(Message::user("anchor"));
        messages.extend((0..20).map(|_| Message::user("short")));
        assert_eq!(compaction_range(&messages, Some(1)), 0..1);
    }

    struct SummaryProvider;

    impl Provider for SummaryProvider {
        fn stream_once(
            &self,
            request: &Request<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<(), Error> {
            assert!(!request.prompt_cache_control);
            assert!(request.prompt_cache_key.is_none());
            on_event(Event::TextDelta("earlier work".into()));
            on_event(Event::Done);
            Ok(())
        }
    }

    #[test]
    fn split_never_orphans_tool_results() {
        // 12 messages; the boundary at len-10 lands on a tool result, so it
        // must walk back to include the assistant that issued the call.
        let mut messages = vec![Message::user("a"), Message::user("b")];
        messages.push(Message::assistant(
            "".into(),
            vec![ToolCall {
                id: "x".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            }],
        ));
        messages.push(Message::tool_result("x", "shell", "r".into(), false));
        for i in 0..8 {
            messages.push(Message::user(format!("m{i}")));
        }
        assert_eq!(messages.len(), 12);
        let split = split_point(&messages);
        // len-10 = 2 → assistant-with-call at 2 is NOT Tool, so split stays 2
        // and the call+result pair stays intact in the tail.
        assert_eq!(split, 2);
        assert!(messages[split].role != Role::Tool);
    }

    #[test]
    fn threshold_math() {
        assert!(!should_compact(0, 100_000, 0.85));
        assert!(!should_compact(84_999, 100_000, 0.85));
        assert!(should_compact(85_000, 100_000, 0.85));
    }

    #[test]
    fn applying_summary_reuses_the_tail_unchanged() {
        let mut messages = (0..12)
            .map(|index| Message::user(format!("message {index}")))
            .collect::<Vec<_>>();

        apply_summary(&mut messages, "earlier work", 2);

        assert_eq!(messages.len(), 11);
        assert!(messages[0].content.contains("earlier work"));
        assert_eq!(messages[1].content, "message 2");
        assert_eq!(messages[10].content, "message 11");
    }

    #[test]
    fn compaction_preserves_a_protected_user_prompt() {
        let messages = (0..24)
            .map(|index| Message::user(format!("message {index}")))
            .collect::<Vec<_>>();

        let range = compaction_range(&messages, Some(3));

        assert_eq!(range, 4..14);
        assert!(!range.contains(&3));
    }

    #[test]
    fn applying_a_middle_summary_keeps_the_undo_anchor() {
        let mut messages = (0..24)
            .map(|index| Message::user(format!("message {index}")))
            .collect::<Vec<_>>();

        apply_summary_range(&mut messages, "goal progress", 4..14);

        assert_eq!(messages[3].content, "message 3");
        assert!(messages[4].content.contains("goal progress"));
        assert_eq!(messages[5].content, "message 14");
        assert_eq!(
            messages.last().map(|message| message.content.as_str()),
            Some("message 23")
        );
    }

    #[test]
    fn compact_keeps_its_public_mutation_contract() -> Result<(), Error> {
        let mut messages = (0..12)
            .map(|index| Message::user(format!("message {index}")))
            .collect::<Vec<_>>();

        let (summary, replaced) =
            compact(&SummaryProvider, "test", 100, &mut messages, &mut |_| {})?;

        assert_eq!(summary, "earlier work");
        assert_eq!(replaced, 2);
        assert!(messages[0].content.contains("earlier work"));
        assert_eq!(messages[1].content, "message 2");
        Ok(())
    }
}
