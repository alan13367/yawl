//! Compact, tool-aware transcript rendering.

use serde_json::Value;

use super::markdown;

const OUTPUT_PREVIEW_LINES: usize = 10;
const CALL_PREVIEW_LINES: usize = 6;
const SUCCESS_BACKGROUND: &str = "\x1b[48;2;42;50;41m";
const ERROR_BACKGROUND: &str = "\x1b[48;2;50;42;42m";

#[derive(Clone, Copy)]
enum Tone {
    Header,
    Output,
    Muted,
    Added,
    Removed,
    Error,
}

struct ToolLine {
    text: String,
    tone: Tone,
}

impl ToolLine {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            text: text.into(),
            tone,
        }
    }
}

pub(super) fn render(
    name: &str,
    args: &str,
    output: &str,
    is_error: bool,
    running: bool,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let width = width.max(8);
    let horizontal_padding = usize::from(width >= 3);
    let content_width = width.saturating_sub(horizontal_padding * 2).max(1);
    let parsed = serde_json::from_str::<Value>(args).ok();
    let mut lines = render_call(name, parsed.as_ref(), args, running, is_error, expanded);

    if should_show_output(name, output, is_error) {
        lines.push(ToolLine::new("", Tone::Output));
        let output_lines = text_lines(output, if is_error { Tone::Error } else { Tone::Output });
        let keep_tail = name == "shell";
        lines.extend(preview_lines(
            output_lines,
            OUTPUT_PREVIEW_LINES,
            expanded,
            keep_tail,
        ));
    }

    let background = if running {
        "\x1b[48;5;58m"
    } else if is_error {
        ERROR_BACKGROUND
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

fn render_call(
    name: &str,
    args: Option<&Value>,
    raw_args: &str,
    running: bool,
    is_error: bool,
    expanded: bool,
) -> Vec<ToolLine> {
    let status = if running {
        "  [running]"
    } else if is_error {
        "  [error]"
    } else {
        ""
    };
    match name {
        "shell" => {
            let command = string_arg(args, "command").unwrap_or(raw_args);
            let mut call = prefixed_lines(command, "$ ", "  ", Tone::Header);
            if let Some(first) = call.first_mut() {
                first.text.push_str(status);
            }
            preview_lines(call, CALL_PREVIEW_LINES, expanded, false)
        }
        "read_file" => vec![ToolLine::new(
            format!(
                "read {}{status}",
                display_path(string_arg(args, "path").unwrap_or("?"))
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
            let mut call = vec![ToolLine::new(format!("edit {path}{status}"), Tone::Header)];
            let old = string_arg(args, "old_string");
            let new = string_arg(args, "new_string");
            if old.is_some() || new.is_some() {
                call.push(ToolLine::new("", Tone::Output));
                let diff = edit_diff(old.unwrap_or_default(), new.unwrap_or_default());
                call.extend(preview_lines(diff, CALL_PREVIEW_LINES, expanded, false));
            }
            call
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

fn should_show_output(name: &str, output: &str, is_error: bool) -> bool {
    if output.is_empty() || output == "(no output; command succeeded)" {
        return false;
    }
    is_error || !matches!(name, "write_file" | "edit_file")
}

fn string_arg<'a>(args: Option<&'a Value>, key: &str) -> Option<&'a str> {
    args?.get(key)?.as_str()
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Equal,
    Added,
    Removed,
}

struct DiffLine<'a> {
    text: &'a str,
    kind: DiffKind,
}

fn edit_diff(old: &str, new: &str) -> Vec<ToolLine> {
    const CONTEXT: usize = 2;
    let old_lines = old.lines().collect::<Vec<_>>();
    let new_lines = new.lines().collect::<Vec<_>>();
    let diff = line_diff(&old_lines, &new_lines);
    let mut rendered = vec![ToolLine::new(
        format!("@@ -{} +{} @@", old_lines.len(), new_lines.len()),
        Tone::Muted,
    )];
    let mut omitted = false;
    for (index, line) in diff.iter().enumerate() {
        let nearby_change = line.kind != DiffKind::Equal
            || diff[index.saturating_sub(CONTEXT)..(index + CONTEXT + 1).min(diff.len())]
                .iter()
                .any(|candidate| candidate.kind != DiffKind::Equal);
        if !nearby_change {
            if !omitted {
                rendered.push(ToolLine::new("  … unchanged lines …", Tone::Muted));
                omitted = true;
            }
            continue;
        }
        omitted = false;
        let (prefix, tone) = match line.kind {
            DiffKind::Equal => ("  ", Tone::Output),
            DiffKind::Added => ("+ ", Tone::Added),
            DiffKind::Removed => ("- ", Tone::Removed),
        };
        rendered.push(ToolLine::new(format!("{prefix}{}", line.text), tone));
    }
    rendered
}

fn line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffLine<'a>> {
    const MAX_CELLS: usize = 250_000;
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        return coarse_line_diff(old, new);
    }
    let columns = new.len() + 1;
    let mut lcs = vec![0_u32; (old.len() + 1) * columns];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let here = old_index * columns + new_index;
            lcs[here] = if old[old_index] == new[new_index] {
                lcs[(old_index + 1) * columns + new_index + 1] + 1
            } else {
                lcs[(old_index + 1) * columns + new_index]
                    .max(lcs[old_index * columns + new_index + 1])
            };
        }
    }
    let mut diff = Vec::with_capacity(old.len() + new.len());
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() || new_index < new.len() {
        if old_index < old.len() && new_index < new.len() && old[old_index] == new[new_index] {
            diff.push(DiffLine {
                text: old[old_index],
                kind: DiffKind::Equal,
            });
            old_index += 1;
            new_index += 1;
        } else if old_index < old.len()
            && (new_index == new.len()
                || lcs[(old_index + 1) * columns + new_index]
                    >= lcs[old_index * columns + new_index + 1])
        {
            diff.push(DiffLine {
                text: old[old_index],
                kind: DiffKind::Removed,
            });
            old_index += 1;
        } else {
            diff.push(DiffLine {
                text: new[new_index],
                kind: DiffKind::Added,
            });
            new_index += 1;
        }
    }
    diff
}

fn coarse_line_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffLine<'a>> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    old[..prefix]
        .iter()
        .map(|text| DiffLine {
            text,
            kind: DiffKind::Equal,
        })
        .chain(old[prefix..old.len() - suffix].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Removed,
        }))
        .chain(new[prefix..new.len() - suffix].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Added,
        }))
        .chain(old[old.len() - suffix..].iter().map(|text| DiffLine {
            text,
            kind: DiffKind::Equal,
        }))
        .collect()
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
        lines.push(ToolLine::new("[Ctrl+O to collapse]", Tone::Muted));
        return lines;
    }
    let marker = if keep_tail {
        format!("... ({omitted} earlier lines, Ctrl+O to expand)")
    } else {
        format!("... ({omitted} more lines, Ctrl+O to expand)")
    };
    if keep_tail {
        let mut preview = Vec::with_capacity(limit + 1);
        preview.push(ToolLine::new(marker, Tone::Muted));
        preview.extend(lines.drain(omitted..));
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
    let chunks = if expanded {
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
    fn compact_shell_preview_keeps_the_tail() {
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
            80,
            false,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("10 earlier lines"));
        assert!(!plain.contains("line 1 "));
        assert!(plain.contains("line 20"));
    }

    #[test]
    fn success_and_error_blocks_use_muted_backgrounds() {
        let success = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
            40,
            false,
        );
        let error = render(
            "shell",
            r#"{"command":"false"}"#,
            "failed",
            true,
            false,
            40,
            false,
        );

        assert!(success.iter().all(|line| line.contains(SUCCESS_BACKGROUND)));
        assert!(error.iter().all(|line| line.contains(ERROR_BACKGROUND)));
    }

    #[test]
    fn tool_blocks_have_background_padding_above_and_below_the_text() {
        let rendered = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
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
            80,
            true,
        );
        let plain = markdown::strip_ansi(&rendered.join("\n"));
        assert!(plain.contains("line 1 "));
        assert!(plain.contains("line 20"));
        assert!(plain.contains("Ctrl+O to collapse"));
    }

    #[test]
    fn shell_output_is_gray_beneath_the_bold_command() {
        let rendered = render(
            "shell",
            r#"{"command":"printf result"}"#,
            "result",
            false,
            false,
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
    fn tool_blocks_have_horizontal_padding() {
        let rendered = render(
            "shell",
            r#"{"command":"true"}"#,
            "",
            false,
            false,
            40,
            false,
        );
        let plain = markdown::strip_ansi(&rendered[1]);
        assert!(plain.starts_with(" $ true"));
    }

    #[test]
    fn edit_diff_preserves_context_and_marks_only_changed_lines() {
        let rendered = edit_diff("before\nold\nafter", "before\nnew\nafter");
        let plain = rendered
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(plain.contains("  before"));
        assert!(plain.contains("- old"));
        assert!(plain.contains("+ new"));
        assert!(plain.contains("  after"));
    }

    #[test]
    fn edit_diff_elides_distant_unchanged_lines() {
        let old = (0..20).map(|line| line.to_string()).collect::<Vec<_>>();
        let mut new = old.clone();
        new[10] = "changed".into();
        let rendered = edit_diff(&old.join("\n"), &new.join("\n"));

        assert!(rendered.iter().any(|line| line.text.contains("unchanged")));
    }
}
