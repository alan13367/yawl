//! Diff pane rendering, syntax highlighting, wrapping, and scroll anchors.

use super::sanitize_plain;
use super::{DiffKind, DiffLine, LoadedDiff};
use crate::tui::{ViewState, markdown};

fn language_for_path(path: &str) -> &str {
    match path.rsplit('.').next().unwrap_or_default() {
        "rs" => "rust",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" => "cpp",
        "sh" | "bash" | "zsh" => "bash",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "html" | "htm" => "html",
        "css" => "css",
        _ => "",
    }
}

/// Scroll offset that puts the first added/removed line near the top of the
/// diff pane, keeping two context lines above it for orientation.
fn first_change_scroll(lines: &[DiffLine]) -> usize {
    lines
        .iter()
        .position(|line| matches!(line.kind, DiffKind::Added | DiffKind::Removed))
        .map(|index| index.saturating_sub(2))
        .unwrap_or(0)
}

pub(super) fn render_diff_pane(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
    let Some(view) = state.git_view.as_mut() else {
        return vec![String::new(); height];
    };
    let Some(diff) = view.diff.as_mut() else {
        return vec![String::new(); height];
    };
    let kind = if diff.commit.is_some() {
        "commit"
    } else if diff.untracked {
        "untracked"
    } else if diff.staged {
        "staged"
    } else {
        "unstaged"
    };
    let header = if diff.commit.is_some() {
        format!(
            "◉ {} · {kind} · +{} −{} \x1b[2m(Esc close)\x1b[0m",
            sanitize_plain(&diff.path),
            diff.added,
            diff.removed
        )
    } else {
        format!(
            "✕ {} · {kind} · +{} −{} \x1b[2m(Esc close)\x1b[0m",
            sanitize_plain(&diff.path),
            diff.added,
            diff.removed
        )
    };
    let mut lines = vec![markdown::fit_width(
        &format!("\x1b[1m{header}\x1b[0m"),
        width,
    )];
    if diff.binary {
        lines.push(markdown::fit_width(
            "\x1b[2mBinary file, diff not shown.\x1b[0m",
            width,
        ));
    } else if diff.lines.is_empty() && !diff.truncated {
        lines.push(markdown::fit_width(
            "\x1b[2mNo content changes (mode change only?).\x1b[0m",
            width,
        ));
    } else {
        let language = language_for_path(&diff.path);
        let body_height = height.saturating_sub(1 + usize::from(diff.truncated));
        // Gutter: 4-wide old number + space + 4-wide new number + space +
        // prefix + space = 12 columns. Long lines wrap onto further screen
        // rows with a blank gutter; only actual lines show numbers.
        let code_width = width.saturating_sub(DIFF_GUTTER_WIDTH).max(8);
        // Plain chunks locate the scroll window without paying for
        // syntax highlighting off-screen.
        let plain: Vec<Vec<String>> = diff
            .lines
            .iter()
            .map(|line| diff_plain_chunks(line, code_width, width))
            .collect();
        if diff.scroll == SCROLL_ANCHOR_PENDING {
            let anchor = first_change_scroll(&diff.lines).min(diff.lines.len());
            diff.scroll = plain.iter().take(anchor).map(Vec::len).sum();
        }
        let total: usize = plain.iter().map(Vec::len).sum();
        let max_top = total.saturating_sub(body_height);
        diff.scroll = diff.scroll.min(max_top);
        let top = diff.scroll;
        let mut visual = 0usize;
        for (line, chunks) in diff.lines.iter().zip(&plain) {
            let remaining = body_height.saturating_sub(lines.len() - 1);
            if remaining == 0 {
                break;
            }
            let start = top.saturating_sub(visual);
            visual += chunks.len();
            if start >= chunks.len() {
                continue;
            }
            let styled = diff_styled_chunks(line, language, code_width, width);
            lines.extend(render_diff_chunks(
                line,
                width,
                styled.iter().enumerate().skip(start).take(remaining),
            ));
        }
    }
    while lines.len() < height {
        lines.push(" ".repeat(width));
    }
    lines.truncate(height);
    if diff.truncated
        && let Some(footer) = lines.last_mut()
    {
        *footer = markdown::fit_width(
            "\x1b[2m… diff truncated; counts are partial …\x1b[0m",
            width,
        );
    }
    lines
}

/// Gutter before diff code: 4-wide old number + space + 4-wide new number +
/// space + prefix + space. Wrapped continuations repeat the width as blanks.
const DIFF_GUTTER_WIDTH: usize = 12;

/// Marks a diff opened but not yet rendered: the first-change anchor still
/// needs the pane width (long lines wrap), so it resolves on the next render.
/// Explicit scrolling before that render cancels the anchor to the top.
pub(super) const SCROLL_ANCHOR_PENDING: usize = usize::MAX;

/// Resolves a pending first-change anchor to an explicit offset before manual
/// scrolling; explicit input always wins over the auto-anchor.
pub(super) fn resolve_diff_scroll(diff: &mut LoadedDiff) {
    if diff.scroll == SCROLL_ANCHOR_PENDING {
        diff.scroll = 0;
    }
}

/// Plain-text screen chunks of one diff line for width-aware wrapping.
/// Hunks use the full row width (no gutter); code lines use `code_width`.
fn diff_plain_chunks(line: &DiffLine, code_width: usize, width: usize) -> Vec<String> {
    let (text, chunk_width) = match line.kind {
        DiffKind::Hunk => (format!("{}…", line.text), width.max(1)),
        _ => (sanitize_plain(&line.text), code_width.max(1)),
    };
    let mut chunks = crate::tui::markdown::split_chars(&text, chunk_width);
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

#[cfg(test)]
fn render_diff_line(
    line: &DiffLine,
    language: &str,
    code_width: usize,
    width: usize,
) -> Vec<String> {
    let chunks = diff_styled_chunks(line, language, code_width, width);
    render_diff_chunks(line, width, chunks.iter().enumerate())
}

fn diff_styled_chunks(
    line: &DiffLine,
    language: &str,
    code_width: usize,
    width: usize,
) -> Vec<String> {
    if line.kind == DiffKind::Hunk {
        return diff_plain_chunks(line, code_width, width);
    }
    // Tokenize the source line before wrapping so comments, strings, and
    // split identifiers keep their style on continuation rows. Only source
    // lines intersecting the viewport reach this function.
    let highlighted = crate::tui::highlight::render_line(language, &line.text);
    markdown::wrap_ansi_hard(&highlighted, code_width.max(1))
}

fn render_diff_chunks<'a>(
    line: &DiffLine,
    width: usize,
    chunks: impl Iterator<Item = (usize, &'a String)>,
) -> Vec<String> {
    const DIM: &str = "\x1b[2m";
    const HUNK: &str = "\x1b[2;36m";
    const RESET: &str = "\x1b[0m";
    // Full-row tinted backgrounds so added/removed lines read at a glance.
    const ADD_BG: &str = "\x1b[48;2;30;64;39m";
    const ADD_FG: &str = "\x1b[38;2;215;236;217m";
    const ADD_PREFIX: &str = "\x1b[1;38;5;114m";
    const DEL_BG: &str = "\x1b[48;2;68;34;34m";
    const DEL_FG: &str = "\x1b[38;2;242;216;216m";
    const DEL_PREFIX: &str = "\x1b[1;38;5;203m";
    if line.kind == DiffKind::Hunk {
        return chunks
            .map(|(_, chunk)| markdown::fit_width(&format!("{HUNK}{chunk}{RESET}"), width))
            .collect();
    }
    let old = line
        .old_no
        .map_or("    ".to_string(), |n| format!("{n:>4}"));
    let new = line
        .new_no
        .map_or("    ".to_string(), |n| format!("{n:>4}"));
    if line.kind == DiffKind::Context {
        return chunks
            .map(|(index, highlighted)| {
                if index == 0 {
                    markdown::fit_width(&format!("{DIM}{old} {new}  {RESET} {highlighted}"), width)
                } else {
                    markdown::fit_width(
                        &format!("{}{highlighted}", " ".repeat(DIFF_GUTTER_WIDTH)),
                        width,
                    )
                }
            })
            .collect();
    }
    let (bg, fg, prefix_style, prefix) = match line.kind {
        DiffKind::Added => (ADD_BG, ADD_FG, ADD_PREFIX, "+"),
        DiffKind::Removed => (DEL_BG, DEL_FG, DEL_PREFIX, "-"),
        // Hunk and Context lines return above; naming them keeps the match
        // exhaustive so a new `DiffKind` variant fails to compile here.
        DiffKind::Hunk | DiffKind::Context => (DEL_BG, DEL_FG, DEL_PREFIX, "-"),
    };
    chunks
        .map(|(index, highlighted)| {
            // Keep the background alive across syntax-highlight resets.
            let rearm = format!("{RESET}{bg}{fg}");
            let body = highlighted.replace(RESET, &rearm);
            if index == 0 {
                let inner = format!("{old} {new} {prefix_style}{prefix}{RESET}{bg}{fg} {body}");
                let visible = markdown::visible_width(&inner);
                if visible > width {
                    return markdown::fit_width(&inner, width);
                }
                format!("{bg}{fg}{inner}{}\x1b[0m", " ".repeat(width - visible))
            } else {
                let inner = format!("{bg}{fg}{}{body}", " ".repeat(DIFF_GUTTER_WIDTH));
                let visible = DIFF_GUTTER_WIDTH + markdown::visible_width(&body);
                if visible > width {
                    return markdown::fit_width(&inner, width);
                }
                format!("{inner}{}\x1b[0m", " ".repeat(width - visible))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lines_fit_narrow_widths() {
        let line = DiffLine {
            old_no: Some(12),
            new_no: Some(13),
            kind: DiffKind::Added,
            text: "fn main() {}".to_string(),
        };
        let rows = render_diff_line(&line, "rust", 8, 20);
        assert!(!rows.is_empty());
        for row in &rows {
            assert!(markdown::visible_width(row) <= 20);
        }
        assert!(markdown::strip_ansi(&rows[0]).contains('+'));
    }

    #[test]
    fn long_diff_lines_wrap_with_blank_continuation_gutters() {
        let added = DiffLine {
            old_no: None,
            new_no: Some(7),
            kind: DiffKind::Added,
            text: "abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
        };
        let rows = render_diff_line(&added, "", 8, 20);
        // 36 chars at width 8: five screen rows, numbers only on the first.
        assert_eq!(rows.len(), 5);
        for row in &rows {
            assert_eq!(markdown::visible_width(row), 20);
        }
        let first = markdown::strip_ansi(&rows[0]);
        assert!(first.contains('7'), "first row keeps its line number");
        assert!(first.contains('+'));
        for continuation in rows.iter().skip(1).map(|row| markdown::strip_ansi(row)) {
            assert!(
                !continuation.chars().take(12).any(|c| c.is_ascii_digit()),
                "continuations show no numbers, got {continuation:?}"
            );
            assert!(
                !continuation.contains('+'),
                "continuations show no prefix, got {continuation:?}"
            );
        }
        // The wrapped chunks reassemble the full line.
        let body: String = rows
            .iter()
            .map(|row| {
                markdown::strip_ansi(row)
                    .chars()
                    .skip(12)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(body, added.text);

        let context = DiffLine {
            old_no: Some(3),
            new_no: Some(3),
            kind: DiffKind::Context,
            text: "abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
        };
        let rows = render_diff_line(&context, "", 8, 20);
        assert_eq!(rows.len(), 5);
        let first = markdown::strip_ansi(&rows[0]);
        assert!(first.contains('3'));
        for continuation in rows.iter().skip(1).map(|row| markdown::strip_ansi(row)) {
            assert!(
                !continuation.chars().take(12).any(|c| c.is_ascii_digit()),
                "context continuations show no numbers, got {continuation:?}"
            );
        }
    }

    #[test]
    fn changed_diff_rows_paint_full_width_backgrounds() {
        let added = DiffLine {
            old_no: None,
            new_no: Some(3),
            kind: DiffKind::Added,
            text: "let x = 1;".to_string(),
        };
        let removed = DiffLine {
            old_no: Some(3),
            new_no: None,
            kind: DiffKind::Removed,
            text: "let x = 0;".to_string(),
        };
        let context = DiffLine {
            old_no: Some(2),
            new_no: Some(2),
            kind: DiffKind::Context,
            text: "let y = 2;".to_string(),
        };
        let added_rows = render_diff_line(&added, "rust", 30, 44);
        let removed_rows = render_diff_line(&removed, "rust", 30, 44);
        let context_rows = render_diff_line(&context, "rust", 30, 44);
        assert_eq!(added_rows.len(), 1);
        assert_eq!(removed_rows.len(), 1);
        assert_eq!(context_rows.len(), 1);
        let (added_row, removed_row, context_row) =
            (&added_rows[0], &removed_rows[0], &context_rows[0]);
        assert!(added_row.contains("48;2"));
        assert!(removed_row.contains("48;2"));
        assert!(!context_row.contains("48;2"));
        // Background spans the whole row, not just the code fragment.
        assert_eq!(markdown::visible_width(added_row), 44);
        assert_eq!(markdown::visible_width(removed_row), 44);
        // Syntax highlighting survives under the background tint.
        assert!(added_row.contains("1;34m") || added_row.contains("32m"));
    }

    #[test]
    fn first_change_scroll_lands_near_the_change() {
        let lines = vec![
            DiffLine {
                old_no: None,
                new_no: None,
                kind: DiffKind::Hunk,
                text: "@@ -1,5 +1,5 @@".into(),
            },
            DiffLine {
                old_no: Some(1),
                new_no: Some(1),
                kind: DiffKind::Context,
                text: "a".into(),
            },
            DiffLine {
                old_no: Some(2),
                new_no: Some(2),
                kind: DiffKind::Context,
                text: "b".into(),
            },
            DiffLine {
                old_no: Some(3),
                new_no: Some(3),
                kind: DiffKind::Context,
                text: "c".into(),
            },
            DiffLine {
                old_no: Some(4),
                new_no: None,
                kind: DiffKind::Removed,
                text: "d".into(),
            },
        ];
        // First change at index 4, two context lines kept above it.
        assert_eq!(first_change_scroll(&lines), 2);
        assert_eq!(first_change_scroll(&[]), 0);
    }
}
