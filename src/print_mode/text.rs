//! Plain-text presentation: the reply on stdout, everything else on stderr.

use std::io::{self, Write};

use crate::agent::TurnEvent;
use crate::provider::{Reasoning, ReasoningKind};

/// Streams turn output to stdout and reasoning to stderr. Reused by the
/// subagent delivery pump so follow-up turns print exactly like the main
/// one.
pub(super) struct TextSink {
    stdout: io::Stdout,
    hide_reasoning: bool,
    pending_reasoning: Vec<Reasoning>,
    response_has_text: bool,
    pub(super) output_error: Option<io::Error>,
}

impl TextSink {
    pub(super) fn new(hide_reasoning: bool) -> Self {
        Self {
            stdout: io::stdout(),
            hide_reasoning,
            pending_reasoning: Vec::new(),
            response_has_text: false,
            output_error: None,
        }
    }

    pub(super) fn handle(&mut self, event: &TurnEvent<'_>) {
        match *event {
            TurnEvent::TextDelta(text) | TurnEvent::AssistantReplace(text) => {
                print_reasoning(&mut self.pending_reasoning);
                if self.output_error.is_none() {
                    let text = sanitize_terminal(text);
                    match self
                        .stdout
                        .write_all(text.as_bytes())
                        .and_then(|()| self.stdout.flush())
                    {
                        Ok(()) => self.response_has_text = true,
                        Err(error) => {
                            self.output_error = Some(error);
                            crate::set_interrupted(true);
                        }
                    }
                }
            }
            TurnEvent::ReasoningDelta { kind, text } => {
                if !self.hide_reasoning {
                    append_reasoning(&mut self.pending_reasoning, kind, &sanitize_terminal(text));
                }
            }
            TurnEvent::RetryReset => {
                eprintln!("\nretry restarted the response; earlier partial text may repeat");
                self.pending_reasoning.clear();
                self.response_has_text = false;
            }
            TurnEvent::Retrying {
                attempt,
                delay_ms,
                ref error,
            } => {
                eprintln!("\nrequest attempt {attempt} failed ({error}); retrying in {delay_ms}ms")
            }
            TurnEvent::AssistantDone => {
                print_reasoning(&mut self.pending_reasoning);
                if self.response_has_text && self.output_error.is_none() {
                    if let Err(error) = self
                        .stdout
                        .write_all(b"\n")
                        .and_then(|()| self.stdout.flush())
                    {
                        self.output_error = Some(error);
                        crate::set_interrupted(true);
                    }
                    self.response_has_text = false;
                }
            }
            TurnEvent::SteerAccepted { .. } => {}
            TurnEvent::Compacting => eprintln!("compacting conversation..."),
            TurnEvent::Compacted { replaced } => eprintln!("compacted {replaced} messages"),
            TurnEvent::PlanHandoff { path, .. } => {
                eprintln!("implementing from saved plan {}", sanitize_terminal(path))
            }
            TurnEvent::PlanPhase(_) => {}
            TurnEvent::Warning(ref text) => eprintln!("{}", sanitize_terminal(text)),
            TurnEvent::WaitingForSubagents { active } => {
                let noun = if active == 1 { "subagent" } else { "subagents" };
                eprintln!("waiting for {active} {noun}…");
            }
            TurnEvent::ToolPreparing { .. }
            | TurnEvent::ToolStart { .. }
            | TurnEvent::ToolEnd { .. }
            | TurnEvent::Usage { .. } => {}
        }
    }
}

impl TextSink {
    /// Ends a reply that stopped mid-line, so notes on stderr start on a
    /// line of their own.
    pub(super) fn end_partial_response(&mut self) {
        if self.response_has_text && self.output_error.is_none() {
            let _ = self
                .stdout
                .write_all(b"\n")
                .and_then(|()| self.stdout.flush());
            self.response_has_text = false;
        }
    }
}

/// Replaces every control character that could steer the terminal with a
/// replacement glyph, preserving the newlines and tabs that make up plain
/// output. The TUI applies the same rule through its renderer; print mode
/// writes straight to the stream, so it has to sanitize here.
fn sanitize_terminal(text: &str) -> std::borrow::Cow<'_, str> {
    crate::text::sanitize_block(text)
}

fn append_reasoning(reasoning: &mut Vec<Reasoning>, kind: ReasoningKind, text: &str) {
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

fn print_reasoning(reasoning: &mut Vec<Reasoning>) {
    for block in reasoning.drain(..) {
        match block.kind {
            ReasoningKind::Summary => {
                for summary in ReasoningKind::summary_parts(&block.content) {
                    eprintln!("{summary}");
                }
            }
            ReasoningKind::Full => eprintln!("{}", block.content.trim()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_sanitization_keeps_text_and_drops_control_sequences() {
        let clean = "hello\nworld\t!";
        assert!(matches!(
            sanitize_terminal(clean),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(sanitize_terminal(clean), clean);

        let hostile = "safe\x1b]0;pwned\x07\rtext";
        let sanitized = sanitize_terminal(hostile);
        assert!(!sanitized.contains('\x1b'));
        assert!(!sanitized.contains('\x07'));
        assert!(!sanitized.contains('\r'));
        assert!(sanitized.contains("safe"));
        assert!(sanitized.contains("text"));
    }
}
