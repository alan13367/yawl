use std::time::Duration;
use std::time::Instant;

#[cfg(test)]
use super::types::reasoning_duration_ms;
use super::types::reasoning_durations_data;
use super::{Event, Provider, Reasoning, ReasoningKind, Request, TokenUsage};
use crate::error::Error;

/// Accumulated result of one assistant response.
#[derive(Debug, Default)]
pub struct TurnOutput {
    pub text: String,
    pub reasoning: Vec<Reasoning>,
    pub tool_calls: Vec<super::ToolCall>,
    pub usage: TokenUsage,
    pub provider_data: Vec<serde_json::Value>,
}

/// Out-of-band notices from the retry wrapper, for display.
pub enum StreamNotice<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
    },
    /// The provider has selected a tool and is still streaming its arguments.
    ToolPreparing {
        name: &'a str,
    },
    /// A retry is about to restart the request from scratch; the consumer
    /// must discard any partial text it displayed.
    RetryReset,
    Retrying {
        attempt: u32,
        delay_ms: u64,
        error: String,
    },
}

const MAX_ATTEMPTS: u32 = 3;

/// Streams one assistant response with retries: exponential backoff, up to
/// 3 attempts, on 429/5xx and I/O failures (including mid-stream
/// disconnects). A retry restarts the whole request; `RetryReset` tells the
/// consumer to drop partial output.
pub fn stream_turn(
    provider: &dyn Provider,
    req: &Request<'_>,
    sink: &mut dyn FnMut(StreamNotice<'_>),
) -> Result<TurnOutput, Error> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut out = TurnOutput::default();
        // Open thinking segment: its index in `out.reasoning` plus the
        // instant its first delta arrived. Scoped to the attempt so a retry
        // discards timing along with the partial output.
        let mut open_reasoning: Option<(usize, Instant)> = None;
        let mut reasoning_durations_ms = Vec::new();
        let result = provider.stream_once(req, &mut |event| match event {
            Event::TextDelta(t) => {
                close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning);
                sink(StreamNotice::TextDelta(&t));
                out.text.push_str(&t);
            }
            Event::ReasoningDelta { kind, text } => {
                // Continuation requires a segment that is actually open: a
                // text or tool boundary may have closed the last record even
                // though its kind matches, and appending to it would merge
                // separate segments and keep the stale duration.
                let continuing = open_reasoning.is_some_and(|(index, _)| {
                    out.reasoning
                        .get(index)
                        .is_some_and(|current| current.kind == kind)
                });
                if continuing {
                    sink(StreamNotice::ReasoningDelta { kind, text: &text });
                    super::append_reasoning(&mut out.reasoning, kind, &text);
                } else {
                    close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning);
                    open_reasoning = Some((out.reasoning.len(), Instant::now()));
                    sink(StreamNotice::ReasoningDelta { kind, text: &text });
                    out.reasoning.push(Reasoning {
                        kind,
                        content: text,
                    });
                    reasoning_durations_ms.push(None);
                }
            }
            Event::ToolCallName(name) => {
                close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning);
                sink(StreamNotice::ToolPreparing { name: &name });
            }
            Event::ToolCall(tc) => {
                close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning);
                out.tool_calls.push(tc);
            }
            Event::Usage(usage) => out.usage = usage,
            Event::ProviderData(value) => out.provider_data.push(value),
            Event::Done => {
                close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning)
            }
        });
        if result.is_ok() {
            close_reasoning_segment(&mut reasoning_durations_ms, &mut open_reasoning);
            if let Some(data) = reasoning_durations_data(&reasoning_durations_ms) {
                out.provider_data.push(data);
            }
        }
        match result {
            Ok(()) => return Ok(out),
            Err(_) if crate::cancellation::interrupted() => return Err(Error::Interrupted),
            Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => {
                let delay_ms = 500u64 << (attempt - 1);
                sink(StreamNotice::Retrying {
                    attempt,
                    delay_ms,
                    error: e.to_string(),
                });
                std::thread::sleep(Duration::from_millis(delay_ms));
                if crate::cancellation::interrupted() {
                    return Err(Error::Interrupted);
                }
                sink(StreamNotice::RetryReset);
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) fn append_reasoning(reasoning: &mut Vec<Reasoning>, kind: ReasoningKind, text: &str) {
    if let Some(current) = reasoning.last_mut()
        && current.kind == kind
    {
        current.content.push_str(text);
    } else {
        reasoning.push(Reasoning {
            kind,
            content: text.to_string(),
        });
    }
}

/// Freezes the wall-clock duration of the open thinking segment, if any.
fn close_reasoning_segment(durations_ms: &mut [Option<u64>], open: &mut Option<(usize, Instant)>) {
    if let Some((index, started)) = open.take()
        && let Some(duration_ms) = durations_ms.get_mut(index)
    {
        *duration_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct FlakyProvider {
        calls: Cell<u32>,
    }

    impl Provider for FlakyProvider {
        fn stream_once(
            &self,
            _req: &Request<'_>,
            on_event: &mut dyn FnMut(Event),
        ) -> Result<(), Error> {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            if call == 1 {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: "partial thought".into(),
                });
                on_event(Event::TextDelta("partial".into()));
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "disconnected",
                )));
            }
            on_event(Event::ReasoningDelta {
                kind: ReasoningKind::Full,
                text: "complete thought".into(),
            });
            on_event(Event::TextDelta("complete".into()));
            on_event(Event::ToolCallName("write_file".into()));
            on_event(Event::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 8,
                cache_write_input_tokens: 0,
                cache_details_reported: true,
            }));
            on_event(Event::Done);
            Ok(())
        }
    }

    #[test]
    fn retries_discard_partial_attempt_output() {
        crate::set_interrupted(false);
        let provider = FlakyProvider {
            calls: Cell::new(0),
        };
        let request = Request {
            model: "test",
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 10,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: Some("session-test"),
        };
        let mut notices = Vec::new();
        let output = stream_turn(&provider, &request, &mut |notice| match notice {
            StreamNotice::TextDelta(text) => notices.push(format!("text:{text}")),
            StreamNotice::ReasoningDelta { kind, text } => {
                notices.push(format!("reasoning:{kind:?}:{text}"));
            }
            StreamNotice::ToolPreparing { name } => {
                notices.push(format!("preparing:{name}"));
            }
            StreamNotice::RetryReset => notices.push("reset".into()),
            StreamNotice::Retrying { attempt, .. } => {
                notices.push(format!("retry:{attempt}"));
            }
        })
        .expect("second attempt should succeed");
        assert_eq!(provider.calls.get(), 2);
        assert_eq!(output.text, "complete");
        assert_eq!(output.reasoning[0].content, "complete thought");
        assert!(
            reasoning_duration_ms(&output.provider_data, 0).is_some(),
            "the text delta closes the thinking segment and freezes its duration"
        );
        assert_eq!(output.usage.cached_input_tokens, 8);
        assert_eq!(
            notices,
            [
                "reasoning:Full:partial thought",
                "text:partial",
                "retry:1",
                "reset",
                "reasoning:Full:complete thought",
                "text:complete",
                "preparing:write_file"
            ]
        );
        assert_eq!(
            (output.usage.input_tokens, output.usage.output_tokens),
            (10, 2)
        );
    }

    #[test]
    fn thinking_segments_freeze_durations_at_their_boundaries() {
        crate::set_interrupted(false);
        struct SegmentedProvider;
        impl Provider for SegmentedProvider {
            fn stream_once(
                &self,
                _req: &Request<'_>,
                on_event: &mut dyn FnMut(Event),
            ) -> Result<(), Error> {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Summary,
                    text: "summarizing".into(),
                });
                // A kind switch closes the summary segment and opens a new one.
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: "detail one".into(),
                });
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: " detail two".into(),
                });
                // A tool call closes the full-thinking segment without text.
                on_event(Event::ToolCallName("shell".into()));
                on_event(Event::ToolCall(super::super::ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: "{}".into(),
                }));
                on_event(Event::Done);
                Ok(())
            }
        }

        let request = Request {
            model: "test",
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 10,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: None,
        };

        let output =
            stream_turn(&SegmentedProvider, &request, &mut |_| {}).expect("stream should succeed");

        assert_eq!(output.reasoning.len(), 2);
        assert!(
            reasoning_duration_ms(&output.provider_data, 0).is_some(),
            "the kind switch freezes the summary segment"
        );
        assert!(
            reasoning_duration_ms(&output.provider_data, 1).is_some(),
            "the tool call freezes the full-thinking segment"
        );
    }

    #[test]
    fn a_resumed_same_kind_segment_after_a_boundary_stays_separate() {
        crate::set_interrupted(false);
        struct ResumeProvider;
        impl Provider for ResumeProvider {
            fn stream_once(
                &self,
                _req: &Request<'_>,
                on_event: &mut dyn FnMut(Event),
            ) -> Result<(), Error> {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: "before".into(),
                });
                on_event(Event::TextDelta("answer".into()));
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Full,
                    text: "after".into(),
                });
                on_event(Event::Done);
                Ok(())
            }
        }

        let request = Request {
            model: "test",
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 10,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: None,
        };

        let output =
            stream_turn(&ResumeProvider, &request, &mut |_| {}).expect("stream should succeed");

        assert_eq!(output.reasoning.len(), 2, "the text delta is a boundary");
        assert_eq!(output.reasoning[0].content, "before");
        assert_eq!(output.reasoning[1].content, "after");
        assert!(reasoning_duration_ms(&output.provider_data, 0).is_some());
        assert!(
            reasoning_duration_ms(&output.provider_data, 1).is_some(),
            "the resumed segment gets its own timer"
        );
    }

    #[test]
    fn successful_stream_without_done_still_freezes_reasoning_duration() {
        crate::set_interrupted(false);
        struct NoDoneProvider;
        impl Provider for NoDoneProvider {
            fn stream_once(
                &self,
                _req: &Request<'_>,
                on_event: &mut dyn FnMut(Event),
            ) -> Result<(), Error> {
                on_event(Event::ReasoningDelta {
                    kind: ReasoningKind::Summary,
                    text: "thinking".into(),
                });
                Ok(())
            }
        }

        let request = Request {
            model: "test",
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 10,
            supports_images: false,
            prompt_cache_control: true,
            prompt_cache_key: None,
        };
        let output =
            stream_turn(&NoDoneProvider, &request, &mut |_| {}).expect("stream should succeed");

        assert!(reasoning_duration_ms(&output.provider_data, 0).is_some());
    }
}
