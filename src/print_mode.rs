use std::io::{self, Read, Write};

use yawl::agent::{Agent, TurnEvent};
use yawl::provider::{Reasoning, ReasoningKind};

pub(super) fn read_prompt(
    words: Vec<String>,
    stdin_is_terminal: bool,
) -> Result<String, io::Error> {
    let positional = words.join(" ");
    let mut piped = String::new();
    if !stdin_is_terminal {
        io::stdin().read_to_string(&mut piped)?;
    }
    let prompt = match (positional.is_empty(), piped.is_empty()) {
        (false, false) => format!("{positional}\n{piped}"),
        (false, true) => positional,
        (true, false) => piped,
        (true, true) => String::new(),
    };
    if prompt.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "prompt is empty",
        ))
    } else {
        Ok(prompt)
    }
}

pub(super) fn run(agent: &mut Agent, prompt: String) -> Result<i32, Box<dyn std::error::Error>> {
    agent.set_print_mode();
    let mut sink = PrintSink::new(agent.config().hide_reasoning);
    let completed = agent.run_turn(Some(prompt), &mut |event| sink.handle(event));
    if let Some(error) = sink.take_output_error() {
        return Err(Box::new(error));
    }
    let completed = completed?;
    if !completed {
        eprintln!("turn interrupted");
        return Ok(130);
    }
    // Print mode has no event loop for background delivery, so settled
    // subagent results are pumped here before the process exits.
    let pumped = agent.pump_subagent_results(&mut |event| sink.handle(event), 30)?;
    if let Some(error) = sink.take_output_error() {
        return Err(Box::new(error));
    }
    if !pumped {
        eprintln!("subagent delivery interrupted");
        return Ok(130);
    }
    Ok(0)
}

/// Streams turn output to stdout and reasoning to stderr. Reused by the
/// subagent delivery pump so follow-up turns print exactly like the main
/// one.
struct PrintSink {
    stdout: io::Stdout,
    hide_reasoning: bool,
    pending_reasoning: Vec<Reasoning>,
    response_has_text: bool,
    output_error: Option<io::Error>,
}

impl PrintSink {
    fn new(hide_reasoning: bool) -> Self {
        Self {
            stdout: io::stdout(),
            hide_reasoning,
            pending_reasoning: Vec::new(),
            response_has_text: false,
            output_error: None,
        }
    }

    fn take_output_error(&mut self) -> Option<io::Error> {
        self.output_error.take()
    }

    fn handle(&mut self, event: TurnEvent<'_>) {
        match event {
            TurnEvent::TextDelta(text) => {
                print_reasoning(&mut self.pending_reasoning);
                if self.output_error.is_none() {
                    match self
                        .stdout
                        .write_all(text.as_bytes())
                        .and_then(|()| self.stdout.flush())
                    {
                        Ok(()) => self.response_has_text = true,
                        Err(error) => {
                            self.output_error = Some(error);
                            yawl::set_interrupted(true);
                        }
                    }
                }
            }
            TurnEvent::ReasoningDelta { kind, text } => {
                if !self.hide_reasoning {
                    append_reasoning(&mut self.pending_reasoning, kind, text);
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
                error,
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
                        yawl::set_interrupted(true);
                    }
                    self.response_has_text = false;
                }
            }
            TurnEvent::Compacting => eprintln!("compacting conversation..."),
            TurnEvent::Compacted { replaced } => eprintln!("compacted {replaced} messages"),
            TurnEvent::Warning(text) => eprintln!("{text}"),
            TurnEvent::ToolPreparing { .. }
            | TurnEvent::ToolStart { .. }
            | TurnEvent::ToolEnd { .. }
            | TurnEvent::Usage { .. } => {}
        }
    }
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
                for summary in reasoning_summary_parts(&block.content) {
                    eprintln!("{summary}");
                }
            }
            ReasoningKind::Full => eprintln!("{}", block.content.trim()),
        }
    }
}

fn reasoning_summary_parts(content: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}
