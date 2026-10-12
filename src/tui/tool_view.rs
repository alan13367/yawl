//! Compact, tool-aware transcript rendering.
//!
//! This facade owns the card types and entry points TUI callers use. The
//! private children own per-tool call content (`calls`), card painting and
//! wrapping (`card`), and the edit-diff algorithm (`diff`).

mod calls;
mod card;
mod diff;

use std::time::Duration;

use serde_json::Value;

use super::theme;
pub(super) use calls::home_relative;
use calls::{
    bool_arg, capitalize_label, define_tool_lines, parse_background_output, parse_background_start,
    parse_background_stop, question_call, render_call, render_question_answers, saved_file_output,
    should_show_output,
};
use card::{card_content_width, paint_card, preview_lines, text_lines};
pub(super) use diff::edit_diff;

const OUTPUT_PREVIEW_LINES: usize = 10;
const CALL_PREVIEW_LINES: usize = 6;
const SHELL_COMMAND_PREVIEW_ROWS: usize = 12;
const DIFF_PREVIEW_LINES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    Header,
    Output,
    Muted,
    Added,
    Removed,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ToolLine {
    pub(super) text: String,
    pub(super) tone: Tone,
    pub(super) wrap: bool,
    /// Columns of blank gutter before each continuation row of a wrapped
    /// line, so diff text stays clear of the `+`/`-` marker column.
    pub(super) hang: usize,
}

/// Display name and liveness for a subagent ID referenced by a tool card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SubagentLabel {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) active: bool,
}

impl ToolLine {
    pub(super) fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
            wrap: false,
            hang: 0,
        }
    }

    fn wrapping(mut self) -> Self {
        self.wrap = true;
        self
    }

    /// Wraps even in a collapsed card, up to a few rows there, indenting
    /// continuation rows by `hang`.
    fn hanging(mut self, hang: usize) -> Self {
        self.wrap = true;
        self.hang = hang;
        self
    }
}

#[expect(clippy::too_many_arguments)]
pub(super) fn render(
    name: &str,
    args: &str,
    output: &str,
    is_error: bool,
    running: bool,
    elapsed: Option<Duration>,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    render_labeled(
        name,
        args,
        output,
        is_error,
        running,
        elapsed,
        &[],
        width,
        expanded,
    )
}

#[expect(clippy::too_many_arguments)]
pub(super) fn render_labeled(
    name: &str,
    args: &str,
    output: &str,
    is_error: bool,
    running: bool,
    elapsed: Option<Duration>,
    labels: &[SubagentLabel],
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let width = width.max(8);
    let content_width = card_content_width(width);
    let parsed = serde_json::from_str::<Value>(args).ok();
    let background_start = name == "shell" && bool_arg(parsed.as_ref(), "background") == Some(true);
    let background_details = if background_start && !is_error && !running {
        parse_background_start(output)
    } else {
        None
    };
    let background_output = if name == "shell_output" && !is_error && !running {
        parse_background_output(output)
    } else {
        None
    };
    let background_stop = if name == "shell_stop" && !is_error && !running {
        parse_background_stop(output)
    } else {
        None
    };
    let mut lines = if name == crate::tools::USER_INPUT_TOOL_NAME && (running || is_error) {
        question_call(parsed.as_ref(), running, is_error, expanded)
    } else if name == crate::tools::USER_INPUT_TOOL_NAME && !running && !is_error {
        render_question_answers(parsed.as_ref(), output)
            .unwrap_or_else(|| question_call(parsed.as_ref(), false, false, expanded))
    } else if name == crate::tools::DEFINE_TOOL_NAME {
        define_tool_lines(parsed.as_ref(), output, running, is_error, expanded)
    } else {
        render_call(
            name,
            parsed.as_ref(),
            args,
            output,
            background_start,
            background_details.as_ref(),
            running,
            elapsed,
            is_error,
            labels,
            content_width,
            expanded,
        )
    };
    let call_rows = lines.len();

    if let Some(details) = background_details {
        lines.push(ToolLine::new("", Tone::Output));
        let name = details
            .name
            .as_deref()
            .map_or_else(String::new, |name| format!("  \u{b7}  {name}"));
        lines.push(ToolLine::new(
            format!(
                "\u{25cf} Started in background  \u{b7}  pid {}{name}  \u{b7}  /ps to view",
                details.pid
            ),
            Tone::Output,
        ));
    } else if let Some(mut details) = background_output {
        lines.push(ToolLine::new("", Tone::Output));
        lines.push(ToolLine::new(
            format!(
                "● {}  \u{b7}  pid {}",
                capitalize_label(&details.status),
                details.pid
            ),
            Tone::Output,
        ));
        if !details.lines.is_empty() {
            lines.push(ToolLine::new("", Tone::Output));
            lines.extend(preview_lines(
                std::mem::take(&mut details.lines),
                OUTPUT_PREVIEW_LINES,
                expanded,
                true,
            ));
        }
    } else if let Some(status) = background_stop {
        lines.push(ToolLine::new("", Tone::Output));
        lines.push(ToolLine::new(format!("● {status}"), Tone::Output));
    } else if should_show_output(name, output, is_error) {
        lines.push(ToolLine::new("", Tone::Output));
        let readable = if is_error {
            None
        } else {
            saved_file_output(name, parsed.as_ref(), output)
        };
        let output = readable.as_deref().unwrap_or(output);
        let output_lines = text_lines(output, if is_error { Tone::Error } else { Tone::Output });
        lines.extend(preview_lines(
            output_lines,
            OUTPUT_PREVIEW_LINES,
            expanded,
            false,
        ));
    }

    if name == "shell" {
        // Command and output previews share one hint at the bottom of the card.
        let mut hints = Vec::new();
        let mut output_truncated = false;
        let mut row = 0;
        lines.retain(|line| {
            let is_hint = line.tone == Tone::Muted && line.text.contains("Ctrl+O or click to ");
            if is_hint {
                output_truncated |= row >= call_rows;
                hints.push(line.text.clone());
            }
            row += 1;
            !is_hint
        });
        if let Some(hint) = hints.pop() {
            let hint = if expanded || (hints.is_empty() && output_truncated) {
                hint
            } else {
                "... Ctrl+O or click to expand".to_string()
            };
            lines.push(ToolLine::new(hint, Tone::Muted));
        }
    }

    let palette = theme::palette();
    let background = if is_error {
        palette.card_error
    } else if name == "read_skill" {
        palette.card_skill
    } else if name == crate::tools::DEFINE_TOOL_NAME && !running {
        palette.card_toolsmith
    } else if name == crate::tools::USER_INPUT_TOOL_NAME {
        palette.card_question
    } else if running {
        palette.card_running
    } else {
        palette.card_success
    };

    paint_card(lines, background, width, expanded)
}

/// Compact human elapsed time: `42s`, `3m 07s`, `1h 04m`.
pub(super) fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 3600 {
        format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Renders one full-width diff card for a `/diff` file entry, mirroring the
/// edit-call presentation: success background, bold path header, then the
/// precomputed diff lines with preview truncation.
pub(super) fn render_diff_card(
    path: &str,
    lines: &[ToolLine],
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let width = width.max(8);
    let count = |tone| lines.iter().filter(|line| line.tone == tone).count();
    let (added, removed) = (count(Tone::Added), count(Tone::Removed));
    let mut content = vec![ToolLine::new(
        format!("{path}  +{added} −{removed}"),
        Tone::Header,
    )];
    content.extend(preview_lines(
        lines.to_vec(),
        DIFF_PREVIEW_LINES,
        expanded,
        false,
    ));
    paint_card(content, theme::palette().card_success, width, expanded)
}

#[cfg(test)]
mod tests {
    use super::super::markdown;
    use super::*;

    #[test]
    fn question_batch_renders_compact_answers_and_timeout_markers() {
        let args = r#"{"questions":[{"id":"scope","question":"Which scope?","options":[{"label":"Focused","description":"small"},{"label":"Broad","description":"large"}],"recommended":0},{"id":"tests","question":"Which tests?","options":[{"label":"Focused","description":"small"},{"label":"All","description":"large"}],"recommended":1}]}"#;
        let output = r#"{"answers":[{"id":"scope","option_index":0,"label":"Focused","source":"user"},{"id":"tests","option_index":1,"label":"All","source":"timeout"}],"timed_out":true}"#;

        let rendered = render(
            crate::tools::USER_INPUT_TOOL_NAME,
            args,
            output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("Which scope? → Focused"));
        assert!(plain.contains("Which tests? → All [timeout]"));
        assert!(!plain.contains("option_index"));
    }

    #[test]
    fn question_batch_renders_custom_answer_text() {
        let args = r#"{"questions":[{"id":"scope","question":"Which scope?","options":[{"label":"Focused","description":"small"},{"label":"Broad","description":"large"}],"recommended":0}]}"#;
        let output = r#"{"answers":[{"id":"scope","option_index":null,"label":"Other","answer":"Only update keyboard navigation","source":"user"}],"timed_out":false}"#;

        let rendered = render(
            crate::tools::USER_INPUT_TOOL_NAME,
            args,
            output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("Which scope? → Only update keyboard navigation"));
        assert!(!plain.contains("→ Other"));
    }

    #[test]
    fn active_and_invalid_question_cards_never_expose_argument_json() {
        for args in [
            r#"{"questions":[{"question":"What should we build?"}]}"#,
            r#"{"questions":[{"quest"#,
        ] {
            for error in [false, true] {
                for width in [16, 80] {
                    let rendered = render(
                        crate::tools::USER_INPUT_TOOL_NAME,
                        args,
                        "",
                        error,
                        true,
                        None,
                        width,
                        true,
                    );
                    let plain = markdown::strip_ansi(&rendered.join("\n"));
                    assert!(!plain.contains("request_user_input"));
                    assert!(!plain.contains("{\"questions\""));
                    assert!(
                        rendered
                            .iter()
                            .all(|line| markdown::visible_width(line) <= width)
                    );
                }
            }
        }
    }

    #[test]
    fn compact_shell_preview_keeps_the_head() {
        let output = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(
            "shell",
            r#"{"command":"cargo test"}"#,
            &output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("10 more lines"));
        assert!(plain.contains("line 1\n") || plain.contains("line 1 "));
        assert!(plain.contains("line 10"));
        assert!(!plain.contains("line 11"));
        assert!(!plain.contains("line 20"));
    }

    #[test]
    fn shell_output_preview_places_expand_hint_after_the_head() {
        let output = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(
            "shell",
            r#"{"command":"cargo test"}"#,
            &output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.find("line 10").unwrap() < plain.find("click to expand").unwrap());
    }

    #[test]
    fn long_shell_command_is_bounded_by_visible_rows_and_expands_fully() {
        let command = format!("python -c '{}'", "x".repeat(1200));
        let args = serde_json::json!({"command": command}).to_string();
        let compact = render("shell", &args, "done", false, false, None, 40, false);
        let expanded = render("shell", &args, "done", false, false, None, 40, true);
        let compact_plain = markdown::strip_ansi(&compact.join("\n"));
        let expanded_plain = markdown::strip_ansi(&expanded.join("\n"));

        assert!(
            compact.len() <= 18,
            "shell card used {} rows",
            compact.len()
        );
        assert!(compact_plain.contains("$ python -c"));
        assert!(compact_plain.contains("done"));
        assert!(compact_plain.contains("click to expand"));
        assert!(expanded_plain.contains("click to collapse"));
        assert!(expanded_plain.matches('x').count() >= 1200);
    }

    #[test]
    fn shell_command_and_output_share_a_bottom_hint() {
        let command = (1..=25)
            .map(|line| format!("echo command-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let args = serde_json::json!({"command": command}).to_string();
        let output = (1..=20)
            .map(|line| format!("result-{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let compact = render("shell", &args, &output, false, false, None, 80, false);
        let expanded = render("shell", &args, &output, false, false, None, 80, true);
        let compact = markdown::strip_ansi(&compact.join("\n"));
        let expanded = markdown::strip_ansi(&expanded.join("\n"));

        assert!(compact.contains("command-12"));
        assert!(!compact.contains("command-25"));
        assert!(compact.contains("result-1 ") || compact.contains("result-1\n"));
        assert!(compact.contains("result-10"));
        assert!(!compact.contains("result-20"));
        assert_eq!(compact.matches("click to expand").count(), 1);
        assert!(compact.find("result-10").unwrap() < compact.find("click to expand").unwrap());
        assert!(expanded.contains("command-25"));
        assert!(expanded.contains("result-1"));
        assert_eq!(expanded.matches("click to collapse").count(), 1);
    }

    #[test]
    fn shell_command_wraps_fully_while_expansion_only_changes_output() {
        let args = serde_json::json!({
            "command": "command -v chromium || command -v chromium-browser && final-marker"
        })
        .to_string();
        let output = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        let compact = render(
            "shell",
            &args,
            &output,
            false,
            true,
            Some(Duration::from_secs(7)),
            36,
            false,
        );
        let expanded = render(
            "shell",
            &args,
            &output,
            false,
            true,
            Some(Duration::from_secs(7)),
            36,
            true,
        );
        let compact = markdown::strip_ansi(&compact.join("\n"));
        let expanded = markdown::strip_ansi(&expanded.join("\n"));

        for rendered in [&compact, &expanded] {
            assert!(rendered.contains("final-marker"), "{rendered}");
            assert!(rendered.contains("[running 7s]"), "{rendered}");
        }
        let compact_call = compact
            .lines()
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        let expanded_call = expanded
            .lines()
            .skip(1)
            .take_while(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(compact_call, expanded_call);
        assert!(!compact.contains("line 20"));
        assert!(expanded.contains("line 20"));
    }

    #[test]
    fn success_and_error_blocks_use_muted_backgrounds() {
        let success = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
            None,
            40,
            false,
        );
        let error = render(
            "shell",
            r#"{"command":"false"}"#,
            "failed",
            true,
            false,
            None,
            40,
            false,
        );

        assert!(
            success
                .iter()
                .all(|line| line.contains(theme::palette().card_success))
        );
        assert!(
            error
                .iter()
                .all(|line| line.contains(theme::palette().card_error))
        );
    }

    #[test]
    fn skill_blocks_use_a_distinct_background_and_label() {
        let running = render(
            "read_skill",
            r#"{"name":"rust"}"#,
            "",
            false,
            true,
            None,
            40,
            false,
        );
        let plain = markdown::strip_ansi(&running.join("\n"));
        assert!(plain.contains("Skill rust  [running]"));
        assert!(
            running
                .iter()
                .all(|line| line.contains(theme::palette().card_skill))
        );

        let error = render(
            "read_skill",
            r#"{"name":"missing"}"#,
            "not available",
            true,
            false,
            None,
            40,
            false,
        );
        assert!(
            error
                .iter()
                .all(|line| line.contains(theme::palette().card_error))
        );
    }

    #[test]
    fn defined_tools_render_as_a_personalization_card() {
        let args = r#"{"name":"greet","description":"Greet someone by name","command":"printf 'hello %s' {{who}}","scope":"project","params":{"who":{"type":"string"}}}"#;
        let saved = render(
            crate::tools::DEFINE_TOOL_NAME,
            args,
            "Created project tool `greet` at /repo/.yawl/tools/greet.json.\nFrom your next step on, call greet(who: string) instead of running its command through shell.\nIt persists across sessions; delete the file to remove it.",
            false,
            false,
            None,
            60,
            false,
        );
        let plain = markdown::strip_ansi(&saved.join("\n"));
        for expected in [
            "\u{2726} Created project tool greet",
            "Greet someone by name",
            "$ printf 'hello %s' {{who}}",
            "Saved to /repo/.yawl/tools/greet.json",
        ] {
            assert!(plain.contains(expected), "missing {expected}:\n{plain}");
        }
        assert!(!plain.contains("From your next step on"));
        assert!(
            saved
                .iter()
                .all(|line| line.contains(theme::palette().card_toolsmith))
        );

        let rejected = render(
            crate::tools::DEFINE_TOOL_NAME,
            args,
            "Tool not saved: 'shell' is a builtin tool name",
            true,
            false,
            None,
            60,
            false,
        );
        let plain = markdown::strip_ansi(&rejected.join("\n"));
        assert!(plain.contains("Define project tool greet  [error]"));
        assert!(plain.contains("Tool not saved: 'shell' is a builtin tool name"));
        assert!(
            rejected
                .iter()
                .all(|line| line.contains(theme::palette().card_error))
        );
    }

    #[test]
    fn web_calls_have_compact_query_and_url_titles() {
        let search = render(
            "web_search",
            r#"{"query":"Rust 2024 edition"}"#,
            "",
            false,
            false,
            None,
            80,
            false,
        );
        let fetch = render(
            "web_fetch",
            r#"{"url":"https://example.com/guide"}"#,
            "",
            false,
            false,
            None,
            80,
            false,
        );
        assert!(
            markdown::strip_ansi(&search.join("\n")).contains("Search web  ·  Rust 2024 edition")
        );
        assert!(
            markdown::strip_ansi(&fetch.join("\n"))
                .contains("Fetch web  ·  https://example.com/guide")
        );
    }

    #[test]
    fn saved_file_tool_json_renders_as_readable_text() {
        let output = r#"{"note":"skip trees","results":[{"path":"src/main.rs"},{"path":"src/agent.rs"}],"skipped":0,"truncated":true}"#;
        let rendered = render(
            "list_files",
            r#"{"path":"."}"#,
            output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.lines().any(|line| line.trim() == "src/main.rs"));
        assert!(plain.lines().any(|line| line.trim() == "src/agent.rs"));
        assert!(!plain.contains("{\"note\""));
        assert!(plain.contains("truncated"));
        let page = r##"{"content":"# Repository guide\nUse Rust.\n","offset":0,"total_bytes":29,"next_offset":null}"##;
        let rendered = render(
            "read_file",
            r#"{"path":"AGENTS.md","offset":0}"#,
            page,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(
            plain
                .lines()
                .any(|line| line.trim() == "# Repository guide")
        );
        assert!(plain.lines().any(|line| line.trim() == "Use Rust."));
        assert!(!plain.contains("{\"content\""));
        assert!(
            saved_file_output(
                "read_file",
                Some(&serde_json::json!({"path": "data.json"})),
                page
            )
            .is_none(),
            "ordinary JSON files must retain their literal content"
        );
        assert!(saved_file_output("list_files", None, "{broken JSON").is_none());
    }

    fn label(id: &str, name: &str, active: bool) -> SubagentLabel {
        SubagentLabel {
            id: id.into(),
            name: name.into(),
            active,
        }
    }

    #[test]
    fn running_wait_drops_finished_children_from_the_waiting_list() {
        let args = r#"{"ids":["sa-1","sa-2","sa-3"]}"#;
        let wait = |labels: &[SubagentLabel], running: bool| {
            markdown::strip_ansi(
                &render_labeled(
                    "subagent_wait",
                    args,
                    "",
                    false,
                    running,
                    Some(Duration::from_secs(42)),
                    labels,
                    80,
                    false,
                )
                .join("\n"),
            )
        };

        let partial = wait(
            &[label("sa-1", "Otter", false), label("sa-2", "Falcon", true)],
            true,
        );
        assert!(
            partial.contains("Waiting for Falcon, sa-3 · 1 of 3 done · 42s"),
            "{partial}"
        );
        assert!(partial.contains("Finished: Otter"));

        let settled = [
            label("sa-1", "Otter", false),
            label("sa-2", "Falcon", false),
            label("sa-3", "Heron", false),
        ];
        let all = wait(&settled, true);
        assert!(
            all.contains("Collecting results · 3 of 3 done · 42s"),
            "{all}"
        );
        assert!(all.contains("Finished: Otter, Falcon, Heron"));

        let finished = wait(&settled, false);
        assert!(finished.contains("Waiting for Otter, Falcon, Heron"));
        assert!(!finished.contains("done"));
    }

    #[test]
    fn subagent_calls_render_readable_summaries_instead_of_json() {
        let wait = render(
            "subagent_wait",
            r#"{"ids":["sa-1","sa-2","sa-3"],"timeout_secs":300}"#,
            "",
            false,
            true,
            Some(Duration::from_secs(83)),
            80,
            false,
        );
        let plain = markdown::strip_ansi(&wait.join("\n"));
        assert!(plain.contains("Waiting for sa-1, sa-2, sa-3 · 1m 23s"));
        assert!(!plain.contains("{\"ids\""));
        assert!(!plain.contains("timeout_secs"));

        let named = render_labeled(
            "subagent_wait",
            r#"{"ids":["sa-1","sa-2"],"timeout_secs":300}"#,
            "",
            false,
            true,
            Some(Duration::from_secs(27)),
            &[
                label("sa-1", "LucidOtter", true),
                label("sa-2", "SwiftFalcon", true),
            ],
            80,
            false,
        );
        let plain = markdown::strip_ansi(&named.join("\n"));
        assert!(plain.contains("Waiting for LucidOtter, SwiftFalcon · 27s"));
        assert!(!plain.contains("sa-1"));

        let spawn = render(
            "subagent_spawn",
            r##"{"prompt":"# Target\naudit the frontend","required_tools":["read_file"],"name":"auditor","agent":"scout"}"##,
            "",
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&spawn.join("\n"));
        assert!(plain.contains("Spawn auditor (scout)"));
        assert!(plain.contains("audit the frontend"));
        assert!(!plain.contains("required_tools"));

        let send = render(
            "subagent_send",
            r#"{"id":"sa-2","message":"also check the tests"}"#,
            "",
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&send.join("\n"));
        assert!(plain.contains("Message sa-2"));
        assert!(plain.contains("also check the tests"));

        let cancel = render(
            "subagent_cancel",
            r#"{"ids":["sa-1"]}"#,
            "",
            false,
            false,
            None,
            80,
            false,
        );
        assert!(markdown::strip_ansi(&cancel.join("\n")).contains("Cancel sa-1"));

        let list = render("subagent_list", r#"{}"#, "", false, false, None, 80, false);
        assert!(markdown::strip_ansi(&list.join("\n")).contains("Subagents"));
    }

    #[test]
    fn tool_blocks_have_background_padding_above_and_below_the_text() {
        let rendered = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
            None,
            40,
            false,
        );

        assert_eq!(rendered.len(), 3);
        assert!(markdown::strip_ansi(&rendered[0]).trim().is_empty());
        assert_eq!(markdown::strip_ansi(&rendered[1]).trim(), "$ true");
        assert!(markdown::strip_ansi(&rendered[2]).trim().is_empty());
    }

    #[test]
    fn expanded_shell_output_includes_every_line() {
        let output = (1..=20)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(
            "shell",
            r#"{"command":"cargo test"}"#,
            &output,
            false,
            false,
            None,
            80,
            true,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("line 1 "));
        assert!(plain.contains("line 20"));
        assert!(plain.contains("or click to collapse"));
    }

    #[test]
    fn shell_output_is_gray_beneath_the_bold_command() {
        let rendered = render(
            "shell",
            r#"{"command":"printf result"}"#,
            "result",
            false,
            false,
            None,
            40,
            false,
        );
        let command = rendered
            .iter()
            .find(|line| line.contains("$ printf result"))
            .expect("the shell command should be rendered");
        let output = rendered
            .iter()
            .find(|line| markdown::strip_ansi(line).trim() == "result")
            .expect("the shell output should be rendered");

        assert!(command.contains("\x1b[1;97m"));
        assert!(output.contains("\x1b[38;5;245m"));
        assert!(!output.contains("\x1b[1;97m"));
    }

    #[test]
    fn background_shell_start_has_a_compact_running_state() {
        let rendered = render(
            "shell",
            r#"{"command":"npm run dev","background":true,"name":"web"}"#,
            "started bg-1 (pid 4242) as web in the background\nnext_cursor: 0",
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("Background terminal  ·  bg-1"));
        assert!(plain.contains("$ npm run dev"));
        assert!(!plain.contains("$ npm run dev  [started"));
        assert!(plain.contains("● Started in background  ·  pid 4242  ·  web  ·  /ps to view"));
        assert!(!plain.contains("next_cursor"));
        assert!(!plain.contains("started bg-1 (pid"));
    }

    #[test]
    fn background_shell_card_titles_the_terminal_before_its_command() {
        let rendered = render(
            "shell",
            r#"{"command":"cargo test --quiet","background":true,"name":"tests"}"#,
            "started bg-3 (pid 4242) as tests in the background\nnext_cursor: 0",
            false,
            false,
            None,
            80,
            false,
        );
        let rows = rendered
            .iter()
            .map(|line| markdown::strip_ansi(line).trim().to_string())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(rows[0], "Background terminal  ·  bg-3");
        assert_eq!(rows[1], "$ cargo test --quiet");
        assert!(rows[2].starts_with("● Started in background"));
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn in_flight_background_shell_says_what_it_is_starting() {
        let rendered = render(
            "shell",
            r#"{"command":"npm run dev","background":true}"#,
            "",
            false,
            true,
            Some(Duration::from_secs(2)),
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("Background terminal  [starting in background 2s]"));
        assert!(plain.contains("$ npm run dev"));
    }

    #[test]
    fn background_output_hides_tool_protocol_and_keeps_terminal_text() {
        let rendered = render(
            "shell_output",
            r#"{"id":"bg-1","cursor":0,"wait_secs":8}"#,
            "bg-1: running (pid 4242)\n[stderr]\nnpm notice run dev\n[stdout]\n\nVITE ready\nLocal: http://localhost:5173/\nnext_cursor: 97",
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("Background output  ·  bg-1"));
        assert!(plain.contains("● Running  ·  pid 4242"));
        assert!(plain.contains("npm notice run dev"));
        assert!(plain.contains("Local: http://localhost:5173/"));
        assert!(!plain.contains("shell_output"));
        assert!(!plain.contains("wait_secs"));
        assert!(!plain.contains("next_cursor"));
        assert!(!plain.contains("[stdout]"));
        assert!(!plain.contains("[stderr]"));
        assert!(!plain.contains("bg-1: running"));
    }

    #[test]
    fn background_output_preview_places_expand_hint_after_the_tail() {
        let output = (1..=20)
            .map(|line| format!("test case-{line} ... ok"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = format!("bg-1: running (pid 4242)\n[stdout]\n{output}\nnext_cursor: 97");
        let rendered = render(
            "shell_output",
            r#"{"id":"bg-1","cursor":0}"#,
            &output,
            false,
            false,
            None,
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));

        assert!(plain.contains("● Running  ·  pid 4242"));
        assert!(!plain.contains("case-1 "));
        assert!(plain.contains("case-20"));
        assert_eq!(plain.matches("click to expand").count(), 1);
        assert!(plain.find("case-20").unwrap() < plain.find("click to expand").unwrap());
    }

    #[test]
    fn background_management_calls_have_human_readable_titles() {
        let running = render(
            "shell_output",
            r#"{"id":"bg-2","wait_secs":8}"#,
            "",
            false,
            true,
            Some(Duration::from_secs(3)),
            80,
            false,
        );
        let listed = render("shell_list", "{}", "none", false, false, None, 80, false);
        let stopped = render(
            "shell_stop",
            r#"{"id":"bg-2"}"#,
            "stop requested for bg-2",
            false,
            false,
            None,
            80,
            false,
        );

        assert!(
            markdown::strip_ansi(&running.join("\n"))
                .contains("Background output  ·  bg-2  [running 3s]")
        );
        assert!(markdown::strip_ansi(&listed.join("\n")).contains("Background terminals"));
        let stopped = markdown::strip_ansi(&stopped.join("\n"));
        assert!(stopped.contains("Stop background terminal  ·  bg-2"));
        assert!(stopped.contains("● Stop requested"));
        assert!(!stopped.contains("{\"id\""));
    }

    #[test]
    fn tool_blocks_have_horizontal_padding() {
        let rendered = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
            None,
            40,
            false,
        );
        let plain = markdown::strip_ansi(&rendered[1]);
        assert!(plain.starts_with(" $ true"));
    }

    #[test]
    fn elapsed_times_use_compact_human_units() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(187)), "3m 07s");
        assert_eq!(format_elapsed(Duration::from_secs(3_845)), "1h 04m");
    }

    #[test]
    fn a_changed_minified_line_stays_short_until_the_card_expands() {
        let minified = "x".repeat(100_000);
        let lines = edit_diff("short", &minified);

        let compact = render_diff_card("dist/app.js", &lines, 80, false);
        assert!(compact.len() < 16, "{} rows", compact.len());
        assert!(compact.iter().any(|line| line.contains("x…")));

        let expanded = render_diff_card("dist/app.js", &lines, 80, true);
        assert!(expanded.len() > 1_000);
    }

    #[test]
    fn diff_cards_truncate_when_compact_and_expand_fully() {
        let old = "a\n".repeat(20);
        let new = "b\n".repeat(20);
        let lines = edit_diff(old.trim_end(), new.trim_end());
        assert!(lines.len() > DIFF_PREVIEW_LINES);

        let compact = render_diff_card("src/note.txt", &lines, 40, false);
        // At width 40 the marker is char-truncated, but must still name click.
        assert!(
            compact
                .iter()
                .any(|line| line.contains("... (") && line.contains("or click"))
        );
        assert!(
            !compact
                .iter()
                .any(|line| line.contains("or click to collapse]"))
        );

        let expanded = render_diff_card("src/note.txt", &lines, 40, true);
        assert!(
            expanded
                .iter()
                .any(|line| line.contains("or click to collapse]"))
        );
        assert!(expanded.iter().any(|line| line.contains("- a")));
        assert!(expanded.iter().any(|line| line.contains("+ b")));
        assert!(
            expanded
                .iter()
                .any(|line| line.contains("src/note.txt  +20 −20"))
        );
    }

    #[test]
    fn edit_cards_name_the_lines_the_edit_landed_on() {
        let header = |args: &str, output: &str, is_error: bool| {
            let lines = render("edit_file", args, output, is_error, false, None, 80, false);
            markdown::strip_ansi(&lines[1]).trim().to_string()
        };
        let args = r#"{"path":"src/a.rs","old_string":"x","new_string":"y"}"#;
        assert_eq!(
            header(args, "edited src/a.rs (line 42)", false),
            "edit src/a.rs:42"
        );
        assert_eq!(
            header(
                args,
                "edited src/a.rs (lines 42-45); matched CRLF line endings",
                false
            ),
            "edit src/a.rs:42-45"
        );
        assert_eq!(
            header(args, "edited src/a.rs (deleted text at line 7)", false),
            "edit src/a.rs:7"
        );
        let all = r#"{"path":"src/a.rs","old_string":"x","new_string":"y","replace_all":true}"#;
        assert_eq!(
            header(
                all,
                "edited src/a.rs (3 replacements at lines 4, 9, 12)",
                false
            ),
            "edit src/a.rs (3 replacements at lines 4, 9, 12)"
        );
        assert_eq!(
            header(all, "old_string not found in src/a.rs", true),
            "edit src/a.rs (all occurrences)  [error]"
        );

        let card = render(
            "edit_file",
            args,
            "edited src/a.rs (line 42)",
            false,
            false,
            None,
            80,
            false,
        )
        .iter()
        .map(|line| markdown::strip_ansi(line))
        .collect::<Vec<_>>();
        assert!(!card.iter().any(|line| line.contains("@@")));
    }

    #[test]
    fn collapsed_diff_rows_wrap_under_a_blank_gutter() {
        let long = format!("let value = {};", "x".repeat(40));
        let args = serde_json::json!({
            "path": "a.rs",
            "old_string": "let value = 1;",
            "new_string": long,
        })
        .to_string();
        let lines = render(
            "edit_file",
            &args,
            "edited a.rs (line 3)",
            false,
            false,
            None,
            30,
            false,
        )
        .iter()
        .map(|line| markdown::strip_ansi(line).trim_end().to_string())
        .collect::<Vec<_>>();
        let added = lines
            .iter()
            .position(|line| line.starts_with(" + let value"))
            .expect("added row");
        assert!(!lines[added].ends_with('…'));
        assert!(lines[added + 1].starts_with("   x"), "{lines:?}");
        let rejoined = lines[added..]
            .iter()
            .take_while(|line| !line.is_empty())
            .map(|line| line.trim_start_matches([' ', '+']))
            .collect::<String>();
        assert_eq!(rejoined, long);
    }
}
