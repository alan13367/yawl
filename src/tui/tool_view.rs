//! Compact, tool-aware transcript rendering.
//!
//! This facade owns generic tool-card rendering. The private diff child owns
//! the edit-diff algorithm. TUI callers keep the entry points here.

mod diff;

pub(super) use diff::edit_diff;

use std::time::Duration;

use serde_json::Value;

use super::markdown;

const OUTPUT_PREVIEW_LINES: usize = 10;
const CALL_PREVIEW_LINES: usize = 6;
const SHELL_COMMAND_PREVIEW_ROWS: usize = 12;
const DIFF_PREVIEW_LINES: usize = 12;
const SUCCESS_BACKGROUND: &str = "\x1b[48;2;42;50;41m";
const ERROR_BACKGROUND: &str = "\x1b[48;2;50;42;42m";
const SKILL_BACKGROUND: &str = "\x1b[48;2;54;44;82m";

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
}

/// Display name and liveness for a subagent ID referenced by a tool card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SubagentLabel {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) active: bool,
}

struct BackgroundStartDetails {
    id: String,
    pid: String,
    name: Option<String>,
}

struct BackgroundOutputDetails {
    status: String,
    pid: String,
    lines: Vec<ToolLine>,
}

impl ToolLine {
    pub(super) fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
            wrap: false,
        }
    }

    fn wrapping(mut self) -> Self {
        self.wrap = true;
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
    let horizontal_padding = usize::from(width >= 3);
    let content_width = width.saturating_sub(horizontal_padding * 2).max(1);
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
    } else {
        render_call(
            name,
            parsed.as_ref(),
            args,
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

    let background = if is_error {
        ERROR_BACKGROUND
    } else if name == "read_skill" {
        SKILL_BACKGROUND
    } else if name == crate::tools::USER_INPUT_TOOL_NAME {
        "\x1b[48;2;40;44;52m"
    } else if running {
        "\x1b[48;5;58m"
    } else {
        SUCCESS_BACKGROUND
    };

    let padding = format!("{background}{}\x1b[0m", " ".repeat(width));
    let mut rendered = Vec::new();
    rendered.push(padding.clone());
    rendered.extend(lines.into_iter().flat_map(|line| {
        render_line(
            line,
            width,
            content_width,
            horizontal_padding,
            expanded,
            background,
        )
    }));
    rendered.push(padding);
    rendered
}

#[expect(clippy::too_many_arguments)]
fn render_call(
    name: &str,
    args: Option<&Value>,
    raw_args: &str,
    background_start: bool,
    background_details: Option<&BackgroundStartDetails>,
    running: bool,
    elapsed: Option<Duration>,
    is_error: bool,
    labels: &[SubagentLabel],
    content_width: usize,
    expanded: bool,
) -> Vec<ToolLine> {
    let status = if background_start && running {
        match elapsed {
            Some(elapsed) => format!("  [starting in background {}]", format_elapsed(elapsed)),
            None => "  [starting in background]".to_string(),
        }
    } else if background_start && !is_error {
        background_details.map_or_else(
            || "  [background]".to_string(),
            |details| format!("  [started in background \u{b7} {}]", details.id),
        )
    } else if running {
        match elapsed {
            Some(elapsed) => format!("  [running {}]", format_elapsed(elapsed)),
            None => "  [running]".to_string(),
        }
    } else if is_error {
        "  [error]".to_string()
    } else {
        String::new()
    };
    let status = status.as_str();
    match name {
        "shell" => {
            let command = string_arg(args, "command").unwrap_or(raw_args);
            let mut call = prefixed_lines(command, "$ ", "  ", Tone::Header);
            if !background_start && let Some(last) = call.last_mut() {
                last.text.push_str(status);
            }
            // Preview rendered rows, not source lines: a single long script line
            // must not take over the transcript before it is expanded.
            let rows = call
                .into_iter()
                .flat_map(|line| {
                    wrap_chars(&sanitize_line(&line.text), content_width)
                        .into_iter()
                        .map(|text| ToolLine::new(text, Tone::Header))
                })
                .collect::<Vec<_>>();
            let truncated = rows.len() > SHELL_COMMAND_PREVIEW_ROWS;
            let mut call = preview_lines(rows, SHELL_COMMAND_PREVIEW_ROWS, expanded, false);
            if background_start {
                let title = background_details.map_or_else(
                    || format!("Background terminal{status}"),
                    |details| format!("Background terminal  ·  {}", details.id),
                );
                let mut titled = vec![
                    ToolLine::new(title, Tone::Header),
                    ToolLine::new("", Tone::Output),
                ];
                titled.extend(call);
                titled
            } else {
                if truncated && !expanded && !status.is_empty() {
                    call.push(ToolLine::new(status.trim(), Tone::Muted));
                }
                call
            }
        }
        "shell_output" => vec![ToolLine::new(
            format!(
                "Background output  \u{b7}  {}{status}",
                string_arg(args, "id").unwrap_or("?")
            ),
            Tone::Header,
        )],
        "shell_list" => vec![ToolLine::new(
            format!("Background terminals{status}"),
            Tone::Header,
        )],
        "shell_stop" => vec![ToolLine::new(
            format!(
                "Stop background terminal  \u{b7}  {}{status}",
                string_arg(args, "id").unwrap_or("?")
            ),
            Tone::Header,
        )],
        "read_file" => {
            let path = display_path(string_arg(args, "path").unwrap_or("?"));
            let from = args
                .and_then(|args| args.get("start_line"))
                .and_then(Value::as_u64)
                .map_or_else(String::new, |line| format!(" from line {line}"));
            vec![ToolLine::new(
                format!("read {path}{from}{status}"),
                Tone::Header,
            )]
        }
        "read_skill" => vec![ToolLine::new(
            format!("Skill {}{status}", string_arg(args, "name").unwrap_or("?")),
            Tone::Header,
        )],
        "web_search" => vec![ToolLine::new(
            format!(
                "Search web  \u{b7}  {}{status}",
                string_arg(args, "query").unwrap_or("?")
            ),
            Tone::Header,
        )],
        "web_fetch" => vec![ToolLine::new(
            format!(
                "Fetch web  \u{b7}  {}{status}",
                string_arg(args, "url").unwrap_or("?")
            ),
            Tone::Header,
        )],
        "write_file" => {
            let path = display_path(string_arg(args, "path").unwrap_or("?"));
            let mut call = vec![ToolLine::new(format!("write {path}{status}"), Tone::Header)];
            if let Some(content) = string_arg(args, "content") {
                call.push(ToolLine::new("", Tone::Output));
                call.extend(preview_lines(
                    text_lines(content, Tone::Output),
                    CALL_PREVIEW_LINES,
                    expanded,
                    false,
                ));
            }
            call
        }
        "edit_file" => {
            let path = display_path(string_arg(args, "path").unwrap_or("?"));
            let all = if bool_arg(args, "replace_all") == Some(true) {
                " (all occurrences)"
            } else {
                ""
            };
            let mut call = vec![ToolLine::new(
                format!("edit {path}{all}{status}"),
                Tone::Header,
            )];
            let old = string_arg(args, "old_string");
            let new = string_arg(args, "new_string");
            if old.is_some() || new.is_some() {
                call.push(ToolLine::new("", Tone::Output));
                let diff = edit_diff(old.unwrap_or_default(), new.unwrap_or_default());
                call.extend(preview_lines(diff, CALL_PREVIEW_LINES, expanded, false));
            }
            call
        }
        "subagent_spawn" => {
            let agent = string_arg(args, "agent").unwrap_or("default");
            let title = match string_arg(args, "name") {
                Some(name) => format!("Spawn {name} ({agent}){status}"),
                None => format!("Spawn subagent ({agent}){status}"),
            };
            let mut call = vec![ToolLine::new(title, Tone::Header)];
            if let Some(prompt) = string_arg(args, "prompt") {
                call.push(ToolLine::new("", Tone::Output));
                call.extend(preview_lines(
                    text_lines(prompt, Tone::Output),
                    CALL_PREVIEW_LINES,
                    expanded,
                    false,
                ));
            }
            call
        }
        "subagent_send" => {
            let target = labeled_id(string_arg(args, "id").unwrap_or("?"), labels);
            let mut call = vec![ToolLine::new(
                format!("Message {target}{status}"),
                Tone::Header,
            )];
            if let Some(message) = string_arg(args, "message") {
                call.push(ToolLine::new("", Tone::Output));
                call.extend(preview_lines(
                    text_lines(message, Tone::Output),
                    CALL_PREVIEW_LINES,
                    expanded,
                    false,
                ));
            }
            call
        }
        "subagent_wait" if running => running_wait_lines(args, labels, elapsed),
        "subagent_wait" => {
            let targets = labeled_ids(args, labels);
            let title = if is_error {
                format!("Waiting for {targets}  [error]")
            } else {
                format!("Waiting for {targets}")
            };
            vec![ToolLine::new(title, Tone::Header)]
        }
        "subagent_cancel" => vec![ToolLine::new(
            format!("Cancel {}{status}", labeled_ids(args, labels)),
            Tone::Header,
        )],
        "subagent_list" => {
            let title = match string_arg(args, "id") {
                Some(id) => format!("Subagent {}{status}", labeled_id(id, labels)),
                None => format!("Subagents{status}"),
            };
            vec![ToolLine::new(title, Tone::Header)]
        }
        _ => {
            let summary = generic_summary(args, raw_args);
            let title = if summary.is_empty() {
                format!("{name}{status}")
            } else {
                format!("{name}  {summary}{status}")
            };
            vec![ToolLine::new(title, Tone::Header)]
        }
    }
}

/// Older sessions contain the original JSON file-tool envelopes. Decode only
/// those known shapes so resumed transcripts benefit from readable output too.
fn saved_file_output(name: &str, args: Option<&Value>, output: &str) -> Option<String> {
    if !matches!(name, "list_files" | "search_files" | "read_file") {
        return None;
    }
    let value: Value = serde_json::from_str(output).ok()?;
    if name == "read_file" {
        let args = args?;
        if args.get("offset").is_none() && args.get("limit").is_none() {
            return None;
        }
        let content = value.get("content")?.as_str()?;
        let offset = value.get("offset")?.as_u64()?;
        let total = value.get("total_bytes")?.as_u64()?;
        let next = value.get("next_offset")?;
        let continuation = if next.is_null() {
            "EOF".into()
        } else {
            format!("next_offset={}", next.as_u64()?)
        };
        return Some(format!(
            "bytes from {offset} of {total}; {continuation}\n\n{content}"
        ));
    }
    let results = value.get("results")?.as_array()?;
    let mut lines = Vec::new();
    for result in results {
        let path = result.get("path")?.as_str()?;
        if name == "list_files" {
            lines.push(path.to_string());
        } else {
            let line = result.get("line")?.as_u64()?;
            let column = result.get("column")?.as_u64()?;
            let text = result.get("text")?.as_str()?;
            lines.push(format!("{path}:{line}:{column}: {text}"));
        }
    }
    if lines.is_empty() {
        lines.push("No results.".into());
    }
    if value.get("truncated").and_then(Value::as_bool) == Some(true) {
        lines.push("[truncated; narrow path or path_contains]".into());
    }
    if let Some(skipped) = value
        .get("skipped")
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
    {
        lines.push(format!("[skipped {skipped} entries]"));
    }
    Some(lines.join("\n"))
}

fn should_show_output(name: &str, output: &str, is_error: bool) -> bool {
    if output.is_empty() || output == "(no output; command succeeded)" {
        return false;
    }
    is_error
        || !matches!(
            name,
            "write_file" | "edit_file" | crate::tools::USER_INPUT_TOOL_NAME
        )
}

fn question_call(
    args: Option<&Value>,
    running: bool,
    is_error: bool,
    expanded: bool,
) -> Vec<ToolLine> {
    let questions = args
        .and_then(|args| args.get("questions"))
        .and_then(Value::as_array);
    let title = if is_error {
        "Questions · needs attention"
    } else if running {
        "Awaiting your answers"
    } else {
        "Questions"
    };
    let count = questions.map_or(String::new(), |questions| {
        format!(" · {} questions", questions.len())
    });
    let mut lines = vec![ToolLine::new(format!("{title}{count}"), Tone::Header).wrapping()];
    if expanded && let Some(questions) = questions {
        for (index, question) in questions.iter().enumerate() {
            if let Some(text) = question.get("question").and_then(Value::as_str) {
                lines
                    .push(ToolLine::new(format!("{}. {text}", index + 1), Tone::Output).wrapping());
            }
        }
    }
    lines
}

fn render_question_answers(args: Option<&Value>, output: &str) -> Option<Vec<ToolLine>> {
    let questions = args?.get("questions")?.as_array()?;
    let result: Value = serde_json::from_str(output).ok()?;
    let answers = result.get("answers")?.as_array()?;
    let timed_out = result
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut lines = vec![ToolLine::new(
        if timed_out {
            "Questions answered · timed out"
        } else {
            "Questions answered"
        },
        Tone::Header,
    )];
    for question in questions {
        let id = question.get("id").and_then(Value::as_str).unwrap_or("?");
        let prompt = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or(id);
        let answer = answers
            .iter()
            .find(|answer| answer.get("id").and_then(Value::as_str) == Some(id));
        let label = answer
            .and_then(|answer| answer.get("answer").or_else(|| answer.get("label")))
            .and_then(Value::as_str)
            .unwrap_or("?");
        let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
        let timeout = answer
            .and_then(|answer| answer.get("source"))
            .and_then(Value::as_str)
            .filter(|source| *source == "timeout")
            .map_or("", |_| " [timeout]");
        lines.push(ToolLine::new(format!("{prompt} → {label}{timeout}"), Tone::Output).wrapping());
    }
    Some(lines)
}

fn string_arg<'a>(args: Option<&'a Value>, key: &str) -> Option<&'a str> {
    args?.get(key)?.as_str()
}

fn bool_arg(args: Option<&Value>, key: &str) -> Option<bool> {
    args?.get(key)?.as_bool()
}

fn parse_background_start(output: &str) -> Option<BackgroundStartDetails> {
    let line = output.lines().next()?;
    let rest = line.strip_prefix("started ")?;
    let (id, rest) = rest.split_once(" (pid ")?;
    let (pid, suffix) = rest.split_once(')')?;
    let suffix = suffix.strip_suffix(" in the background")?;
    let name = suffix
        .strip_prefix(" as ")
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    if !suffix.is_empty() && name.is_none() {
        return None;
    }
    Some(BackgroundStartDetails {
        id: id.to_string(),
        pid: pid.to_string(),
        name,
    })
}

fn parse_background_output(output: &str) -> Option<BackgroundOutputDetails> {
    let mut lines = output.lines();
    let status_line = lines.next()?;
    let cursor_line = lines.next_back()?;
    cursor_line
        .strip_prefix("next_cursor: ")?
        .parse::<u64>()
        .ok()?;

    let (_, status_and_pid) = status_line.split_once(": ")?;
    let (status, pid) = status_and_pid.rsplit_once(" (pid ")?;
    let pid = pid.strip_suffix(')')?;
    let lines = lines
        .filter_map(|line| match line {
            "[stdout]" | "[stderr]" => None,
            "[earlier output was discarded]" => {
                Some(ToolLine::new("Earlier output was discarded.", Tone::Muted))
            }
            line => Some(ToolLine::new(line, Tone::Output)),
        })
        .collect();

    Some(BackgroundOutputDetails {
        status: status.to_string(),
        pid: pid.to_string(),
        lines,
    })
}

fn capitalize_label(label: &str) -> String {
    let mut characters = label.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn parse_background_stop(output: &str) -> Option<String> {
    if output.starts_with("stop requested for ") {
        return Some("Stop requested".to_string());
    }
    output
        .split_once(": ")
        .map(|(_, status)| capitalize_label(status))
}

fn labeled_id(id: &str, labels: &[SubagentLabel]) -> String {
    labels
        .iter()
        .find(|label| label.id == id)
        .map(|label| label.name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(id)
        .to_string()
}

fn arg_ids(args: Option<&Value>) -> Vec<&str> {
    args.and_then(|args| args.get("ids"))
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn join_labels(ids: &[&str], labels: &[SubagentLabel]) -> String {
    ids.iter()
        .map(|id| labeled_id(id, labels))
        .collect::<Vec<_>>()
        .join(", ")
}

fn labeled_ids(args: Option<&Value>, labels: &[SubagentLabel]) -> String {
    let ids = arg_ids(args);
    if ids.is_empty() {
        "?".to_string()
    } else {
        join_labels(&ids, labels)
    }
}

/// A waited ID counts as finished only when a known child has settled;
/// unknown IDs stay listed so the card never hides work it cannot see.
fn running_wait_lines(
    args: Option<&Value>,
    labels: &[SubagentLabel],
    elapsed: Option<Duration>,
) -> Vec<ToolLine> {
    let ids = arg_ids(args);
    if ids.is_empty() {
        return vec![ToolLine::new("Waiting for ?", Tone::Header)];
    }
    let (done, pending): (Vec<&str>, Vec<&str>) = ids
        .iter()
        .partition(|id| labels.iter().any(|label| label.id == **id && !label.active));
    let mut title = if pending.is_empty() {
        "Collecting results".to_string()
    } else {
        format!("Waiting for {}", join_labels(&pending, labels))
    };
    if !done.is_empty() {
        title.push_str(&format!(" · {} of {} done", done.len(), ids.len()));
    }
    if let Some(elapsed) = elapsed {
        title.push_str(&format!(" · {}", format_elapsed(elapsed)));
    }
    let mut lines = vec![ToolLine::new(title, Tone::Header)];
    if !done.is_empty() {
        lines.push(ToolLine::new(
            format!("Finished: {}", join_labels(&done, labels)),
            Tone::Muted,
        ));
    }
    lines
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

fn generic_summary(args: Option<&Value>, raw_args: &str) -> String {
    if let Some(args) = args {
        for key in ["path", "query", "pattern", "command"] {
            if let Some(value) = args.get(key).and_then(Value::as_str) {
                return display_path(value);
            }
        }
        if let Ok(compact) = serde_json::to_string(args) {
            return compact;
        }
    }
    raw_args.to_string()
}

fn display_path(path: &str) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if path == home {
        "~".to_string()
    } else if let Some(relative) = path
        .strip_prefix(home.as_ref())
        .and_then(|rest| rest.strip_prefix('/'))
    {
        format!("~/{relative}")
    } else {
        path.to_string()
    }
}

fn text_lines(text: &str, tone: Tone) -> Vec<ToolLine> {
    text.lines().map(|line| ToolLine::new(line, tone)).collect()
}

fn prefixed_lines(text: &str, first: &str, rest: &str, tone: Tone) -> Vec<ToolLine> {
    let mut lines = text.lines();
    let Some(line) = lines.next() else {
        return vec![ToolLine::new(first, tone)];
    };
    std::iter::once(ToolLine::new(format!("{first}{line}"), tone))
        .chain(lines.map(|line| ToolLine::new(format!("{rest}{line}"), tone)))
        .collect()
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
    let horizontal_padding = usize::from(width >= 3);
    let content_width = width.saturating_sub(horizontal_padding * 2).max(1);
    let mut content = vec![ToolLine::new(path, Tone::Header)];
    content.extend(preview_lines(
        lines.to_vec(),
        DIFF_PREVIEW_LINES,
        expanded,
        false,
    ));
    let background = SUCCESS_BACKGROUND;
    let padding = format!("{background}{}\x1b[0m", " ".repeat(width));
    let mut rendered = Vec::new();
    rendered.push(padding.clone());
    rendered.extend(content.into_iter().flat_map(|line| {
        render_line(
            line,
            width,
            content_width,
            horizontal_padding,
            expanded,
            background,
        )
    }));
    rendered.push(padding);
    rendered
}

fn preview_lines(
    mut lines: Vec<ToolLine>,
    limit: usize,
    expanded: bool,
    keep_tail: bool,
) -> Vec<ToolLine> {
    if lines.len() <= limit {
        return lines;
    }
    let omitted = lines.len() - limit;
    if expanded {
        lines.push(ToolLine::new("[Ctrl+O or click to collapse]", Tone::Muted));
        return lines;
    }
    let marker = if keep_tail {
        format!("... ({omitted} earlier lines, Ctrl+O or click to expand)")
    } else {
        format!("... ({omitted} more lines, Ctrl+O or click to expand)")
    };
    if keep_tail {
        let mut preview = Vec::with_capacity(limit + 1);
        preview.extend(lines.drain(omitted..));
        preview.push(ToolLine::new(marker, Tone::Muted));
        preview
    } else {
        lines.truncate(limit);
        lines.push(ToolLine::new(marker, Tone::Muted));
        lines
    }
}

fn render_line(
    line: ToolLine,
    width: usize,
    content_width: usize,
    horizontal_padding: usize,
    expanded: bool,
    background: &str,
) -> Vec<String> {
    let sanitized = sanitize_line(&line.text);
    let chunks = if expanded || line.wrap {
        wrap_chars(&sanitized, content_width)
    } else {
        vec![truncate_chars(&sanitized, content_width)]
    };
    let style = match line.tone {
        Tone::Header => "\x1b[1;97m",
        Tone::Output => "\x1b[38;5;245m",
        Tone::Muted => "\x1b[2;37m",
        Tone::Added => "\x1b[38;5;114m",
        Tone::Removed => "\x1b[38;5;203m",
        Tone::Error => "\x1b[38;5;210m",
    };
    let left_pad = " ".repeat(horizontal_padding);
    chunks
        .into_iter()
        .map(|chunk| {
            let right_pad =
                " ".repeat(width.saturating_sub(horizontal_padding + chunk.chars().count()));
            format!("{background}{style}{left_pad}{chunk}{right_pad}\x1b[0m")
        })
        .collect()
}

fn sanitize_line(line: &str) -> String {
    markdown::strip_ansi(line)
        .chars()
        .map(|character| {
            if character == '\t' {
                ' '
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn truncate_chars(text: &str, width: usize) -> String {
    let Some((cut, _)) = text.char_indices().nth(width) else {
        return text.to_string();
    };
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let end = text
        .char_indices()
        .nth(width - 1)
        .map_or(cut, |(index, _)| index);
    let mut truncated = text[..end].to_string();
    truncated.push('…');
    truncated
}

fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let chunks = markdown::split_chars(text, width);
    if chunks.is_empty() {
        return vec![String::new()];
    }
    chunks
}

#[cfg(test)]
mod tests {
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

        assert!(success.iter().all(|line| line.contains(SUCCESS_BACKGROUND)));
        assert!(error.iter().all(|line| line.contains(ERROR_BACKGROUND)));
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
        assert!(running.iter().all(|line| line.contains(SKILL_BACKGROUND)));

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
        assert!(error.iter().all(|line| line.contains(ERROR_BACKGROUND)));
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
        assert!(expanded.iter().any(|line| line.contains("src/note.txt")));
    }
}
