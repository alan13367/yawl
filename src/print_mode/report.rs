//! What a one-shot run produced, collected from its events: the final
//! answer, tool activity, files written, usage, and warnings.

use crate::agent::TurnEvent;
use crate::provider::TokenUsage;

/// Tools whose successful calls change the file named by their `path`
/// argument.
const FILE_WRITING_TOOLS: [&str; 2] = ["write_file", "edit_file"];

#[derive(Debug, Default)]
pub(super) struct Report {
    /// Text of the response being streamed.
    current: String,
    /// The last finished non-empty response.
    last: String,
    pending_path: Option<String>,
    pub(super) tool_calls: u64,
    pub(super) tool_errors: u64,
    pub(super) edited_files: Vec<String>,
    pub(super) warnings: Vec<String>,
    pub(super) requests: u64,
    pub(super) tokens: TokenUsage,
    pub(super) context_tokens: Option<u64>,
}

impl Report {
    /// Records `event`. Returns the full text of a response that just
    /// finished, so streaming output can also report it whole.
    pub(super) fn observe(&mut self, event: &TurnEvent<'_>) -> Option<String> {
        match *event {
            TurnEvent::TextDelta(text) => self.current.push_str(text),
            TurnEvent::AssistantReplace(text) => {
                self.current.clear();
                self.current.push_str(text);
            }
            TurnEvent::RetryReset => self.current.clear(),
            TurnEvent::AssistantDone => {
                let text = std::mem::take(&mut self.current);
                if !text.trim().is_empty() {
                    self.last.clone_from(&text);
                    return Some(text);
                }
            }
            TurnEvent::ToolStart { name, args } => {
                self.pending_path = FILE_WRITING_TOOLS
                    .contains(&name)
                    .then(|| path_argument(args))
                    .flatten();
            }
            TurnEvent::ToolEnd { is_error, .. } => {
                self.tool_calls += 1;
                if is_error {
                    self.tool_errors += 1;
                }
                if let Some(path) = self.pending_path.take()
                    && !is_error
                    && !self.edited_files.contains(&path)
                {
                    self.edited_files.push(path);
                }
            }
            TurnEvent::Warning(ref text) => self.warnings.push(text.clone()),
            TurnEvent::Usage {
                context_tokens,
                request_usage,
                ..
            } => {
                self.requests += 1;
                self.tokens = self.tokens.saturating_add(request_usage);
                self.context_tokens = Some(context_tokens);
            }
            _ => {}
        }
        None
    }

    /// The answer to report: the last finished response, or the partial one
    /// when the run stopped while it streamed.
    pub(super) fn final_text(&self) -> &str {
        if self.current.trim().is_empty() {
            &self.last
        } else {
            &self.current
        }
    }
}

fn path_argument(args: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args).ok()?;
    value
        .get("path")?
        .as_str()
        .filter(|path| !path.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(report: &mut Report, name: &str, args: &str, is_error: bool) {
        report.observe(&TurnEvent::ToolStart { name, args });
        report.observe(&TurnEvent::ToolEnd {
            name,
            output: "",
            images: &[],
            is_error,
        });
    }

    #[test]
    fn final_text_is_the_last_finished_response() {
        let mut report = Report::default();
        report.observe(&TurnEvent::TextDelta("Let me look."));
        assert_eq!(
            report.observe(&TurnEvent::AssistantDone).as_deref(),
            Some("Let me look.")
        );
        tool(&mut report, "shell", r#"{"command":"ls"}"#, false);
        report.observe(&TurnEvent::TextDelta("Fixed "));
        report.observe(&TurnEvent::TextDelta("it."));
        report.observe(&TurnEvent::AssistantDone);
        // An empty closing response does not erase the answer.
        assert_eq!(report.observe(&TurnEvent::AssistantDone), None);
        assert_eq!(report.final_text(), "Fixed it.");
    }

    #[test]
    fn retries_and_replacements_rewrite_the_streamed_text() {
        let mut report = Report::default();
        report.observe(&TurnEvent::TextDelta("partial"));
        report.observe(&TurnEvent::RetryReset);
        report.observe(&TurnEvent::TextDelta("draft"));
        report.observe(&TurnEvent::AssistantReplace("goal result"));
        report.observe(&TurnEvent::AssistantDone);
        assert_eq!(report.final_text(), "goal result");
    }

    #[test]
    fn interrupted_runs_report_the_partial_response() {
        let mut report = Report::default();
        report.observe(&TurnEvent::TextDelta("done"));
        report.observe(&TurnEvent::AssistantDone);
        report.observe(&TurnEvent::TextDelta("half an ans"));
        assert_eq!(report.final_text(), "half an ans");
    }

    #[test]
    fn successful_file_tools_record_each_path_once() {
        let mut report = Report::default();
        tool(
            &mut report,
            "write_file",
            r#"{"path":"a.rs","content":""}"#,
            false,
        );
        tool(&mut report, "edit_file", r#"{"path":"a.rs"}"#, false);
        tool(&mut report, "edit_file", r#"{"path":"b.rs"}"#, true);
        tool(&mut report, "read_file", r#"{"path":"c.rs"}"#, false);
        tool(&mut report, "write_file", "not json", false);
        assert_eq!(report.edited_files, ["a.rs"]);
        assert_eq!((report.tool_calls, report.tool_errors), (5, 1));
    }

    #[test]
    fn usage_accumulates_across_requests() {
        let mut report = Report::default();
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            ..TokenUsage::default()
        };
        for context_tokens in [12, 30] {
            report.observe(&TurnEvent::Usage {
                context_tokens,
                context_window: 100,
                request_usage: usage,
                session_usage: Default::default(),
            });
        }
        assert_eq!(report.requests, 2);
        assert_eq!(report.tokens.input_tokens, 20);
        assert_eq!(report.context_tokens, Some(30));
    }
}
