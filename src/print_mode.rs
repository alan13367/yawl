//! One-shot runs: read the prompt, run one turn plus any subagent
//! follow-ups, and present the result as plain text or as JSON for scripts
//! and other agent harnesses.

mod json;
mod report;
mod text;

use std::io::{self, Read};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::{Agent, TurnEvent};
use crate::cli::Format;
use crate::error::Error;
use crate::exit_status::{self, Failure};

use report::Report;

/// How long yawl waits on standard input before saying on stderr why it is
/// waiting. A harness that leaves stdin open would otherwise hang silently.
const STDIN_HINT_AFTER: Duration = Duration::from_secs(3);

/// Joins the prompt arguments with piped standard input. `deadline` bounds
/// the wait for stdin to close, so `--timeout` covers it too.
pub(super) fn read_prompt(
    words: Vec<String>,
    read_stdin: bool,
    deadline: Option<Duration>,
) -> Result<String, Failure> {
    let positional = words.join(" ");
    let piped = if read_stdin {
        read_stdin_within(deadline)?
    } else {
        String::new()
    };
    let prompt = match (positional.is_empty(), piped.is_empty()) {
        (false, false) => format!("{positional}\n{piped}"),
        (false, true) => positional,
        (true, false) => piped,
        (true, true) => String::new(),
    };
    if prompt.trim().is_empty() {
        Err(Failure::usage("prompt is empty"))
    } else {
        Ok(prompt)
    }
}

/// Reads stdin to its end on a helper thread, so the wait can time out. A
/// read still blocked at the deadline is abandoned; the process exits soon
/// after.
fn read_stdin_within(deadline: Option<Duration>) -> Result<String, Failure> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let result = io::stdin().read_to_string(&mut text).map(|_| text);
        let _ = sender.send(result);
    });
    let waited = |wait: Option<Duration>| match wait {
        Some(wait) => receiver.recv_timeout(wait).ok(),
        None => receiver.recv().ok(),
    };
    let read =
        |result: Result<String, io::Error>| result.map_err(|error| exit_status::classify(&error));
    let first = deadline.map_or(STDIN_HINT_AFTER, |deadline| deadline.min(STDIN_HINT_AFTER));
    if let Some(result) = waited(Some(first)) {
        return read(result);
    }
    let timed_out = || {
        Failure::timeout(
            "timed out waiting for standard input to close; pass --no-stdin when nothing is piped",
        )
    };
    if deadline.is_some_and(|deadline| deadline <= first) {
        return Err(timed_out());
    }
    eprintln!("yawl: waiting for standard input to close; pass --no-stdin when nothing is piped");
    match waited(deadline.map(|deadline| deadline - first)) {
        Some(result) => read(result),
        None => Err(timed_out()),
    }
}

/// How a one-shot run is presented and bounded.
#[derive(Debug, Clone, Default)]
pub(super) struct RunOptions {
    pub(super) format: Format,
    pub(super) timeout: Option<Duration>,
    /// The session is memory-only, so there is no id to resume.
    pub(super) ephemeral: bool,
    /// What opening the session had to skip, reported like any other
    /// warning so JSON callers see it too.
    pub(super) session_warning: Option<String>,
}

/// How the run ended.
enum Outcome {
    Completed,
    Interrupted,
    TimedOut,
    Failed(Failure),
}

impl Outcome {
    fn status(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::TimedOut => "timeout",
            // A failure built before the turn, such as `--timeout` expiring
            // while stdin was read, keeps the status its kind implies.
            Self::Failed(failure) => failure.status(),
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Interrupted => exit_status::INTERRUPTED,
            Self::TimedOut => exit_status::TIMEOUT,
            Self::Failed(failure) => failure.code,
        }
    }
}

/// Runs one turn, then delivers settled subagent results, and reports the
/// outcome. Returns the process exit code. Text mode reports failures on
/// stderr; JSON modes report them in the result object. Only a failure to
/// write the output itself is returned as an error.
pub(super) fn run(
    agent: &mut Agent,
    prompt: String,
    options: RunOptions,
) -> Result<i32, Box<dyn std::error::Error>> {
    agent.set_print_mode();
    let started = Instant::now();
    let mut sink = Sink::new(options.format, agent.config().hide_reasoning);
    if options.format == Format::Jsonl {
        sink.write(&json!({
            "type": "start",
            "session_id": session_id(agent, &options),
            "model": agent.model(),
            "cwd": crate::config::working_dir(),
            "version": env!("CARGO_PKG_VERSION"),
        }));
    }
    if let Some(warning) = &options.session_warning {
        sink.handle(TurnEvent::Warning(warning.clone()));
    }
    agent.clear_cancellation();
    let token = agent.cancellation_token();
    let (result, timed_out) = match options.timeout {
        Some(timeout) => {
            crate::cancellation::with_timeout(&token, timeout, || drive(agent, prompt, &mut sink))
        }
        None => (drive(agent, prompt, &mut sink), false),
    };
    let outcome = match result {
        _ if timed_out => Outcome::TimedOut,
        Ok(true) => Outcome::Completed,
        Ok(false) | Err(Error::Interrupted) => Outcome::Interrupted,
        Err(error) => Outcome::Failed(exit_status::classify_error(&error)),
    };
    if options.ephemeral {
        crate::checkpoint::Checkpoints::remove(&agent.config().home_dir, agent.session_id());
    }
    if let Some(error) = sink.take_output_error() {
        return Err(Box::new(error));
    }
    if options.format.is_machine() {
        let result = result_object(agent, &options, &sink.report, &outcome, started);
        sink.write(&result);
        if let Some(error) = sink.take_output_error() {
            return Err(Box::new(error));
        }
        return Ok(outcome.exit_code());
    }
    if let Output::Text(text) = &mut sink.output {
        text.end_partial_response();
    }
    if !options.ephemeral {
        eprintln!("yawl --session {}", agent.session_id());
    }
    match outcome {
        Outcome::Completed => Ok(0),
        Outcome::Interrupted => {
            eprintln!("turn interrupted");
            Ok(exit_status::INTERRUPTED)
        }
        Outcome::TimedOut => {
            eprintln!("yawl: timed out; continue with --session to resume");
            Ok(exit_status::TIMEOUT)
        }
        Outcome::Failed(failure) => {
            eprintln!("yawl: {}", failure.message);
            if let Some(hint) = failure.hint {
                eprintln!("hint: {hint}");
            }
            Ok(failure.code)
        }
    }
}

/// The turn and then subagent delivery. `Ok(false)` means interrupted.
fn drive(agent: &mut Agent, prompt: String, sink: &mut Sink) -> Result<bool, Error> {
    let completed = agent.run(
        crate::agent::TurnRequest::Prompt(Some(prompt.into())),
        &mut |event| sink.handle(event),
    )?;
    if !completed || sink.has_output_error() {
        return Ok(false);
    }
    // Print mode has no event loop for background delivery, so settled
    // subagent results are pumped here before the process exits.
    agent.pump_subagent_results(&mut |event| sink.handle(event), 30)
}

fn session_id(agent: &Agent, options: &RunOptions) -> Option<String> {
    (!options.ephemeral).then(|| agent.session_id().to_string())
}

fn result_object(
    agent: &Agent,
    options: &RunOptions,
    report: &Report,
    outcome: &Outcome,
    started: Instant,
) -> Value {
    let context = json!({
        "tokens": report.context_tokens.unwrap_or_else(|| agent.context_tokens()),
        "window": agent.context_window(),
    });
    result_json(
        outcome,
        session_id(agent, options),
        Some(agent.model()),
        report,
        started.elapsed(),
        context,
    )
}

/// The one result shape every run ends with, whether or not its turn
/// started, so callers can read each field without checking for it.
fn result_json(
    outcome: &Outcome,
    session_id: Option<String>,
    model: Option<&str>,
    report: &Report,
    duration: Duration,
    context: Value,
) -> Value {
    let error = match outcome {
        Outcome::Failed(failure) => failure.to_json(),
        Outcome::TimedOut => json!({
            "kind": "timeout",
            "message": "the run exceeded --timeout",
            "hint": "resume with --session to continue where it stopped",
        }),
        Outcome::Interrupted => json!({
            "kind": "interrupted",
            "message": "the run was interrupted",
            "hint": Value::Null,
        }),
        Outcome::Completed => Value::Null,
    };
    json!({
        "type": "result",
        "status": outcome.status(),
        "exit_code": outcome.exit_code(),
        "session_id": session_id,
        "model": model,
        "text": report.final_text().trim(),
        "error": error,
        "duration_ms": u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        "requests": report.requests,
        "usage": json::usage(report.tokens),
        "context": context,
        "tool_calls": report.tool_calls,
        "tool_errors": report.tool_errors,
        "edited_files": report.edited_files,
        "warnings": report.warnings,
    })
}

/// The run's presentation plus the report every format builds.
struct Sink {
    output: Output,
    report: Report,
    hide_reasoning: bool,
}

enum Output {
    Text(text::TextSink),
    /// Events are collected; only the result is written.
    Json(json::JsonWriter),
    /// Every event is written as it happens.
    Jsonl(json::JsonWriter),
}

impl Sink {
    fn new(format: Format, hide_reasoning: bool) -> Self {
        Self {
            output: match format {
                Format::Text => Output::Text(text::TextSink::new(hide_reasoning)),
                Format::Json => Output::Json(json::JsonWriter::new()),
                Format::Jsonl => Output::Jsonl(json::JsonWriter::new()),
            },
            report: Report::default(),
            hide_reasoning,
        }
    }

    fn handle(&mut self, event: TurnEvent<'_>) {
        let finished = self.report.observe(&event);
        match &mut self.output {
            Output::Text(sink) => sink.handle(&event),
            Output::Json(_) => {}
            Output::Jsonl(writer) => {
                if let Some(value) = json::event(&event, finished.as_deref(), self.hide_reasoning) {
                    writer.write(&value);
                }
            }
        }
    }

    fn write(&mut self, value: &Value) {
        match &mut self.output {
            Output::Text(_) => {}
            Output::Json(writer) | Output::Jsonl(writer) => writer.write(value),
        }
    }

    fn has_output_error(&self) -> bool {
        match &self.output {
            Output::Text(sink) => sink.output_error.is_some(),
            Output::Json(writer) | Output::Jsonl(writer) => writer.output_error.is_some(),
        }
    }

    fn take_output_error(&mut self) -> Option<io::Error> {
        match &mut self.output {
            Output::Text(sink) => sink.output_error.take(),
            Output::Json(writer) | Output::Jsonl(writer) => writer.output_error.take(),
        }
    }
}

/// The result object for a run that failed before its turn started, such
/// as a missing model, in the shape a completed run reports: counts are
/// zero, lists are empty, and the session and model are null.
pub(super) fn startup_failure(failure: &Failure) -> String {
    let result = result_json(
        &Outcome::Failed(failure.clone()),
        None,
        None,
        &Report::default(),
        Duration::ZERO,
        json!({"tokens": 0, "window": Value::Null}),
    );
    crate::text::json_line(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_failures_have_the_full_result_shape() {
        let line = startup_failure(&Failure::usage("unknown option '--bogus'"));
        let value: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(value["status"], "error");
        assert_eq!(value["exit_code"], exit_status::USAGE);
        assert_eq!(value["error"]["kind"], "usage");
        for field in [
            "session_id",
            "model",
            "text",
            "duration_ms",
            "requests",
            "usage",
            "context",
            "tool_calls",
            "tool_errors",
            "edited_files",
            "warnings",
        ] {
            assert!(value.get(field).is_some(), "missing {field}");
        }
        assert_eq!(value["edited_files"], json!([]));
        assert_eq!(value["usage"]["input_tokens"], 0);

        let timeout = startup_failure(&Failure::timeout("stdin never closed"));
        let value: Value = serde_json::from_str(&timeout).unwrap();
        assert_eq!(value["status"], "timeout");
        assert_eq!(value["exit_code"], exit_status::TIMEOUT);
    }
}
