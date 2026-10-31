//! Machine-readable run output: one JSON value per line, escaped through
//! `crate::text::json_line` so nothing in it can steer a terminal.

use std::io::{self, Write};

use serde_json::{Value, json};

use crate::agent::TurnEvent;
use crate::provider::{ReasoningKind, TokenUsage};

/// Tool output in a `tool_end` event is cut to this many characters; the
/// model still saw all of it.
const TOOL_OUTPUT_CHARS: usize = 4_000;

/// Writes JSON lines to stdout, remembering the first write failure.
pub(super) struct JsonWriter {
    stdout: io::Stdout,
    pub(super) output_error: Option<io::Error>,
}

impl JsonWriter {
    pub(super) fn new() -> Self {
        Self {
            stdout: io::stdout(),
            output_error: None,
        }
    }

    pub(super) fn write(&mut self, value: &Value) {
        if self.output_error.is_some() {
            return;
        }
        let mut line = crate::text::json_line(value);
        line.push('\n');
        if let Err(error) = self
            .stdout
            .write_all(line.as_bytes())
            .and_then(|()| self.stdout.flush())
        {
            self.output_error = Some(error);
            crate::set_interrupted(true);
        }
    }
}

/// The streamed form of `event`, or `None` for events a caller has no use
/// for. `finished` is the full text of a response that just ended.
pub(super) fn event(
    event: &TurnEvent<'_>,
    finished: Option<&str>,
    hide_reasoning: bool,
) -> Option<Value> {
    Some(match *event {
        TurnEvent::TextDelta(text) => json!({"type": "text", "delta": text}),
        TurnEvent::AssistantReplace(text) => json!({"type": "text_replace", "text": text}),
        TurnEvent::AssistantDone => json!({"type": "message", "text": finished?}),
        TurnEvent::ReasoningDelta { kind, text } if !hide_reasoning => json!({
            "type": "reasoning",
            "kind": match kind {
                ReasoningKind::Summary => "summary",
                ReasoningKind::Full => "full",
            },
            "delta": text,
        }),
        TurnEvent::ReasoningDelta { .. } => return None,
        TurnEvent::RetryReset => json!({"type": "retry_reset"}),
        TurnEvent::Retrying {
            attempt,
            delay_ms,
            ref error,
        } => json!({"type": "retry", "attempt": attempt, "delay_ms": delay_ms, "error": error}),
        TurnEvent::ToolStart { name, args } => json!({
            "type": "tool_start",
            "name": name,
            "args": serde_json::from_str::<Value>(args).unwrap_or_else(|_| Value::from(args)),
        }),
        TurnEvent::ToolEnd {
            name,
            output,
            images,
            is_error,
        } => {
            let chars = output.chars().count();
            json!({
                "type": "tool_end",
                "name": name,
                "is_error": is_error,
                "output": crate::text::ellipsize(output, TOOL_OUTPUT_CHARS),
                "output_chars": chars,
                "truncated": chars > TOOL_OUTPUT_CHARS,
                "images": images.len(),
            })
        }
        TurnEvent::Compacting => json!({"type": "compacting"}),
        TurnEvent::Compacted { replaced } => json!({"type": "compacted", "replaced": replaced}),
        TurnEvent::PlanHandoff { path, replaced } => {
            json!({"type": "plan_handoff", "path": path, "replaced": replaced})
        }
        TurnEvent::Warning(ref message) => json!({"type": "warning", "message": message}),
        TurnEvent::WaitingForSubagents { active } => {
            json!({"type": "waiting_for_subagents", "active": active})
        }
        TurnEvent::Usage {
            context_tokens,
            context_window,
            request_usage,
            ..
        } => json!({
            "type": "usage",
            "context_tokens": context_tokens,
            "context_window": context_window,
            "request": usage(request_usage),
        }),
        TurnEvent::ToolPreparing { .. }
        | TurnEvent::SteerAccepted { .. }
        | TurnEvent::PlanPhase(_) => return None,
    })
}

pub(super) fn usage(tokens: TokenUsage) -> Value {
    json!({
        "input_tokens": tokens.input_tokens,
        "output_tokens": tokens.output_tokens,
        "cached_input_tokens": tokens.cached_input_tokens,
        "cache_write_input_tokens": tokens.cache_write_input_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_events_carry_parsed_arguments_and_bounded_output() {
        let start = event(
            &TurnEvent::ToolStart {
                name: "shell",
                args: r#"{"command":"ls"}"#,
            },
            None,
            false,
        )
        .expect("tool start is reported");
        assert_eq!(start["args"]["command"], "ls");

        let output = "x".repeat(TOOL_OUTPUT_CHARS + 10);
        let end = event(
            &TurnEvent::ToolEnd {
                name: "shell",
                output: &output,
                images: &[],
                is_error: false,
            },
            None,
            false,
        )
        .expect("tool end is reported");
        assert_eq!(end["truncated"], true);
        assert_eq!(end["output_chars"], TOOL_OUTPUT_CHARS + 10);
        assert!(end["output"].as_str().unwrap().chars().count() <= TOOL_OUTPUT_CHARS);
    }

    #[test]
    fn unparseable_arguments_are_kept_as_a_string() {
        let start = event(
            &TurnEvent::ToolStart {
                name: "x",
                args: "{oops",
            },
            None,
            false,
        )
        .unwrap();
        assert_eq!(start["args"], "{oops");
    }

    #[test]
    fn finished_messages_and_hidden_reasoning() {
        let done = event(&TurnEvent::AssistantDone, Some("all done"), false).unwrap();
        assert_eq!(done, json!({"type": "message", "text": "all done"}));
        assert!(event(&TurnEvent::AssistantDone, None, false).is_none());
        let reasoning = TurnEvent::ReasoningDelta {
            kind: ReasoningKind::Summary,
            text: "thinking",
        };
        assert!(event(&reasoning, None, true).is_none());
        assert_eq!(event(&reasoning, None, false).unwrap()["kind"], "summary");
    }
}
