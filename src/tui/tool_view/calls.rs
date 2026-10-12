//! Per-tool card content: call headers, argument previews, and readable
//! summaries of results for each builtin tool.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::card::{prefixed_lines, preview_lines, sanitize_line, text_lines, wrap_chars};
use super::{
    CALL_PREVIEW_LINES, SHELL_COMMAND_PREVIEW_ROWS, SubagentLabel, Tone, ToolLine, edit_diff,
    format_elapsed,
};

pub(super) struct BackgroundStartDetails {
    id: String,
    pub(super) pid: String,
    pub(super) name: Option<String>,
}

pub(super) struct BackgroundOutputDetails {
    pub(super) status: String,
    pub(super) pid: String,
    pub(super) lines: Vec<ToolLine>,
}

#[expect(clippy::too_many_arguments)]
pub(super) fn render_call(
    name: &str,
    args: Option<&Value>,
    raw_args: &str,
    output: &str,
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
            let location = if running || is_error {
                None
            } else {
                edit_location(output)
            };
            let location = location.unwrap_or_else(|| {
                if bool_arg(args, "replace_all") == Some(true) {
                    " (all occurrences)".to_string()
                } else {
                    String::new()
                }
            });
            let mut call = vec![ToolLine::new(
                format!("edit {path}{location}{status}"),
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

/// The card for a tool the model saved: what it does, its command, and
/// where it lives. The model-facing usage report stays out of the card.
pub(super) fn define_tool_lines(
    args: Option<&Value>,
    output: &str,
    running: bool,
    is_error: bool,
    expanded: bool,
) -> Vec<ToolLine> {
    let name = string_arg(args, "name").unwrap_or("?");
    let scope = string_arg(args, "scope").unwrap_or("custom");
    let saved = (!running && !is_error)
        .then(|| define_tool_report(output))
        .flatten();
    let title = match (saved, running, is_error) {
        (Some((verb, _)), _, _) => format!("\u{2726} {verb} {scope} tool {name}"),
        (None, true, _) => format!("Define {scope} tool {name}  [running]"),
        (None, _, true) => format!("Define {scope} tool {name}  [error]"),
        (None, false, false) => format!("Define {scope} tool {name}"),
    };
    let mut lines = vec![ToolLine::new(title, Tone::Header)];
    if let Some(description) = string_arg(args, "description") {
        lines.push(ToolLine::new("", Tone::Output));
        lines.extend(text_lines(description.trim(), Tone::Output));
    }
    if let Some(command) = string_arg(args, "command") {
        lines.push(ToolLine::new("", Tone::Output));
        lines.extend(preview_lines(
            prefixed_lines(command, "$ ", "  ", Tone::Output),
            CALL_PREVIEW_LINES,
            expanded,
            false,
        ));
    }
    if let Some((_, path)) = saved {
        lines.push(ToolLine::new("", Tone::Output));
        lines.push(ToolLine::new(
            format!("Saved to {}", display_path(path)),
            Tone::Muted,
        ));
    }
    lines
}

/// Reads `Created|Updated <scope> tool `<name>` at <path>.` from the first
/// line of a successful `define_tool` result.
fn define_tool_report(output: &str) -> Option<(&str, &str)> {
    let (verb, rest) = output.lines().next()?.split_once(' ')?;
    if !matches!(verb, "Created" | "Updated") {
        return None;
    }
    let path = rest.split_once(" at ")?.1.strip_suffix('.')?;
    Some((verb, path))
}

/// Older sessions contain the original JSON file-tool envelopes. Decode only
/// those known shapes so resumed transcripts benefit from readable output too.
pub(super) fn saved_file_output(name: &str, args: Option<&Value>, output: &str) -> Option<String> {
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

pub(super) fn should_show_output(name: &str, output: &str, is_error: bool) -> bool {
    if output.is_empty() || output == "(no output; command succeeded)" {
        return false;
    }
    is_error
        || !matches!(
            name,
            "write_file"
                | "edit_file"
                | crate::tools::USER_INPUT_TOOL_NAME
                | crate::tools::DEFINE_TOOL_NAME
        )
}

pub(super) fn question_call(
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

pub(super) fn render_question_answers(args: Option<&Value>, output: &str) -> Option<Vec<ToolLine>> {
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

pub(super) fn bool_arg(args: Option<&Value>, key: &str) -> Option<bool> {
    args?.get(key)?.as_bool()
}

pub(super) fn parse_background_start(output: &str) -> Option<BackgroundStartDetails> {
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

pub(super) fn parse_background_output(output: &str) -> Option<BackgroundOutputDetails> {
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

pub(super) fn capitalize_label(label: &str) -> String {
    let mut characters = label.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

pub(super) fn parse_background_stop(output: &str) -> Option<String> {
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

/// Where a successful `edit_file` call landed, read from its result:
/// `:42`, `:42-45`, or ` (3 replacements at lines 4, 9, 12)`.
fn edit_location(output: &str) -> Option<String> {
    let (_, detail) = output.strip_prefix("edited ")?.rsplit_once(" (")?;
    let (detail, _) = detail.split_once(')')?;
    let lines = detail
        .strip_prefix("line ")
        .or_else(|| detail.strip_prefix("lines "))
        .or_else(|| detail.strip_prefix("deleted text at line "));
    match lines {
        Some(lines) => Some(format!(":{lines}")),
        None if detail.contains(" replacements at lines ") => Some(format!(" ({detail})")),
        None => None,
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
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home_relative(Path::new(path), home.as_deref())
}

/// Shortens a path under `home` to `~`, leaving other paths unchanged. An
/// empty or root `home` would claim every path, so it is ignored.
pub(in crate::tui) fn home_relative(path: &Path, home: Option<&Path>) -> String {
    let home = home.filter(|home| home.parent().is_some());
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
