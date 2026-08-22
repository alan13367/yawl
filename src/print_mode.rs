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
    let mut stdout = io::stdout().lock();
    let hide_reasoning = agent.config().hide_reasoning;
    let mut pending_reasoning = Vec::new();
    let mut response_has_text = false;
    let mut output_error = None;
    let completed = agent.run_turn(Some(prompt), &mut |event| match event {
        TurnEvent::TextDelta(text) => {
            print_reasoning(&mut pending_reasoning);
            if output_error.is_none() {
                match stdout
                    .write_all(text.as_bytes())
                    .and_then(|()| stdout.flush())
                {
                    Ok(()) => response_has_text = true,
                    Err(error) => {
                        output_error = Some(error);
                        yawl::set_interrupted(true);
                    }
                }
            }
        }
        TurnEvent::ReasoningDelta { kind, text } => {
            if !hide_reasoning {
                append_reasoning(&mut pending_reasoning, kind, text);
            }
        }
        TurnEvent::RetryReset => {
            eprintln!("\nretry restarted the response; earlier partial text may repeat");
            pending_reasoning.clear();
            response_has_text = false;
        }
        TurnEvent::Retrying {
            attempt,
            delay_ms,
            error,
        } => eprintln!("\nrequest attempt {attempt} failed ({error}); retrying in {delay_ms}ms"),
        TurnEvent::AssistantDone => {
            print_reasoning(&mut pending_reasoning);
            if response_has_text && output_error.is_none() {
                if let Err(error) = stdout.write_all(b"\n").and_then(|()| stdout.flush()) {
                    output_error = Some(error);
                    yawl::set_interrupted(true);
                }
                response_has_text = false;
            }
        }
        TurnEvent::Compacting => eprintln!("compacting conversation..."),
        TurnEvent::Compacted { replaced } => {
            eprintln!("compacted {replaced} messages")
        }
        TurnEvent::Warning(text) => eprintln!("{text}"),
        TurnEvent::ToolStart { .. } | TurnEvent::ToolEnd { .. } | TurnEvent::Usage { .. } => {}
    });
    if let Some(error) = output_error {
        return Err(Box::new(error));
    }
    let completed = completed?;
    if completed {
        Ok(0)
    } else {
        eprintln!("turn interrupted");
        Ok(130)
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
                let summary = block
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                eprintln!("{summary}");
            }
            ReasoningKind::Full => eprintln!("{}", block.content.trim()),
        }
    }
}
