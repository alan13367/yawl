use std::time::Duration;

use super::{Event, Provider, Reasoning, ReasoningKind, Request};
use crate::error::Error;

/// Accumulated result of one assistant response.
#[derive(Debug, Default)]
pub struct TurnOutput {
    pub text: String,
    pub reasoning: Vec<Reasoning>,
    pub tool_calls: Vec<super::ToolCall>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub provider_data: Vec<serde_json::Value>,
}

/// Out-of-band notices from the retry wrapper, for display.
pub enum StreamNotice<'a> {
    TextDelta(&'a str),
    ReasoningDelta {
        kind: ReasoningKind,
        text: &'a str,
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
        let result = provider.stream_once(req, &mut |event| match event {
            Event::TextDelta(t) => {
                sink(StreamNotice::TextDelta(&t));
                out.text.push_str(&t);
            }
            Event::ReasoningDelta { kind, text } => {
                sink(StreamNotice::ReasoningDelta { kind, text: &text });
                super::append_reasoning(&mut out.reasoning, kind, &text);
            }
            Event::ToolCall(tc) => out.tool_calls.push(tc),
            Event::Usage {
                input_tokens,
                output_tokens,
            } => {
                out.input_tokens = input_tokens;
                out.output_tokens = output_tokens;
            }
            Event::ProviderData(value) => out.provider_data.push(value),
            Event::Done => {}
        });
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
            on_event(Event::Usage {
                input_tokens: 10,
                output_tokens: 2,
            });
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
        };
        let mut notices = Vec::new();
        let output = stream_turn(&provider, &request, &mut |notice| match notice {
            StreamNotice::TextDelta(text) => notices.push(format!("text:{text}")),
            StreamNotice::ReasoningDelta { kind, text } => {
                notices.push(format!("reasoning:{kind:?}:{text}"));
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
        assert_eq!(
            notices,
            [
                "reasoning:Full:partial thought",
                "text:partial",
                "retry:1",
                "reset",
                "reasoning:Full:complete thought",
                "text:complete"
            ]
        );
        assert_eq!((output.input_tokens, output.output_tokens), (10, 2));
    }
}
