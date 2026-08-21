//! Auto-compaction: when the context is ~85% full (estimated from
//! provider-reported usage), the same model summarizes everything except the
//! system prompt and the last ~10 messages. The session JSONL keeps the full
//! original history; compaction is recorded as an event.

use std::fmt::Write as _;

use crate::error::Error;
use crate::provider::{Message, Provider, Request, Role, StreamNotice, stream_turn};

/// How many trailing messages survive compaction verbatim.
pub const KEEP_TAIL: usize = 10;

const SUMMARY_MARKER: &str = "[conversation summary]";

pub fn summary_message(summary: &str) -> Message {
    Message::user(format!(
        "{SUMMARY_MARKER}\nEarlier conversation, summarized to free context:\n\n{summary}"
    ))
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
    split
}

/// Renders messages as a plain transcript for the summarizer.
fn transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        match m.role {
            Role::User => {
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
                        crate::error::truncate(&tc.arguments, 400)
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
                out.push_str(&crate::error::truncate(&m.content, 2_000));
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
    let (summary, split) = summarize(provider, model, max_tokens, messages, sink)?;
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
    sink: &mut dyn FnMut(StreamNotice<'_>),
) -> Result<(String, usize), Error> {
    let split = split_point(messages);
    if split == 0 {
        return Err(Error::Config(
            "nothing to compact: conversation is too short".into(),
        ));
    }
    let ask = Message::user(format!(
        "Summarize this transcript per your instructions:\n\n{}",
        transcript(&messages[..split])
    ));
    let request = Request {
        model,
        system: SUMMARIZER_SYSTEM,
        messages: std::slice::from_ref(&ask),
        tools: &[],
        max_tokens,
    };
    let out = stream_turn(provider, &request, sink)?;
    if out.text.trim().is_empty() {
        return Err(Error::Protocol("summarizer returned empty text".into()));
    }
    let summary = out.text.trim().to_string();
    Ok((summary, split))
}

pub(crate) fn apply_summary(messages: &mut Vec<Message>, summary: &str, split: usize) {
    drop(messages.splice(..split, std::iter::once(summary_message(summary))));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Event, ToolCall};

    struct SummaryProvider;

    impl Provider for SummaryProvider {
        fn stream_once(
            &self,
            _request: &Request<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<(), Error> {
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
