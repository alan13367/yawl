//! Card painting: previews, padding, wrapping, and tone colors for the lines
//! of a tool card.

use super::super::{markdown, theme};
use super::{Tone, ToolLine};

pub(super) fn text_lines(text: &str, tone: Tone) -> Vec<ToolLine> {
    text.lines().map(|line| ToolLine::new(line, tone)).collect()
}

pub(super) fn prefixed_lines(text: &str, first: &str, rest: &str, tone: Tone) -> Vec<ToolLine> {
    let mut lines = text.lines();
    let Some(line) = lines.next() else {
        return vec![ToolLine::new(first, tone)];
    };
    std::iter::once(ToolLine::new(format!("{first}{line}"), tone))
        .chain(lines.map(|line| ToolLine::new(format!("{rest}{line}"), tone)))
        .collect()
}

/// Columns of padding inside the card background on each side.
fn card_padding(width: usize) -> usize {
    usize::from(width >= 3)
}

pub(super) fn card_content_width(width: usize) -> usize {
    width.saturating_sub(card_padding(width) * 2).max(1)
}

/// Paints `lines` across a full-width `background` with a blank row above
/// and below.
pub(super) fn paint_card(
    lines: Vec<ToolLine>,
    background: &str,
    width: usize,
    expanded: bool,
) -> Vec<String> {
    let padding = format!("{background}{}\x1b[0m", " ".repeat(width));
    let mut rendered = vec![padding.clone()];
    rendered.extend(
        lines
            .into_iter()
            .flat_map(|line| render_line(line, width, expanded, background)),
    );
    rendered.push(padding);
    rendered
}

pub(super) fn preview_lines(
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

/// Rows one hanging line may wrap to in a collapsed card: enough to show a
/// change near the end of an ordinary line, while a changed minified line
/// cannot fill the transcript before the card is expanded.
const COLLAPSED_HANGING_ROWS: usize = 4;

fn render_line(line: ToolLine, width: usize, expanded: bool, background: &str) -> Vec<String> {
    let horizontal_padding = card_padding(width);
    let content_width = card_content_width(width);
    let mut sanitized = sanitize_line(&line.text);
    let capped = !expanded && line.hang > 0;
    if capped
        && let Some((cut, _)) = sanitized
            .char_indices()
            .nth(content_width * COLLAPSED_HANGING_ROWS + 1)
    {
        // Wrap only what can be shown; the rest would be discarded anyway.
        sanitized.truncate(cut);
    }
    let mut chunks = if expanded || line.wrap {
        wrap_hanging(&sanitized, content_width, line.hang)
    } else {
        vec![crate::text::ellipsize(&sanitized, content_width)]
    };
    if capped && chunks.len() > COLLAPSED_HANGING_ROWS {
        chunks.truncate(COLLAPSED_HANGING_ROWS);
        if let Some(last) = chunks.last_mut() {
            last.pop();
            last.push('…');
        }
    }
    let palette = theme::palette();
    let style = match line.tone {
        Tone::Header => palette.tool_header,
        Tone::Output => palette.tool_output,
        Tone::Muted => palette.tool_muted,
        Tone::Added => palette.tool_added,
        Tone::Removed => palette.tool_removed,
        Tone::Error => palette.tool_error,
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

pub(super) fn sanitize_line(line: &str) -> String {
    crate::text::sanitize_line(&markdown::strip_ansi(line), crate::text::Tabs::Space)
}

fn wrap_hanging(text: &str, width: usize, hang: usize) -> Vec<String> {
    let mut chunks = wrap_chars(text, width);
    if hang == 0 || chunks.len() < 2 || width <= hang {
        return chunks;
    }
    let rest = chunks.split_off(1).concat();
    let gutter = " ".repeat(hang);
    chunks.extend(
        markdown::split_chars(&rest, width - hang)
            .into_iter()
            .map(|chunk| format!("{gutter}{chunk}")),
    );
    chunks
}

pub(super) fn wrap_chars(text: &str, width: usize) -> Vec<String> {
    let chunks = markdown::split_chars(text, width);
    if chunks.is_empty() {
        return vec![String::new()];
    }
    chunks
}
