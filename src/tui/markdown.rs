//! Terminal markdown renderer with ANSI styling, box-drawing tables, and
//! chrome-free fenced-code highlighting (code renders as background-shaded
//! highlighted lines so terminal drag-select copies clean commands).

use super::{highlight, theme};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const RESET: &str = "\x1b[0m";
const HEADING: &str = "\x1b[1m";
const BOLD: &str = "\x1b[1m";
const ITALIC: &str = "\x1b[3m";
const STRIKE: &str = "\x1b[9m";
const UNDERLINE: &str = "\x1b[4m";
/// Lists nest visibly without letting deep indentation eat narrow panes.
const MAX_INDENT: usize = 12;
/// Bounds the `<url>` lookahead so text full of `<` stays linear.
const MAX_AUTOLINK_CHARS: usize = 2048;

pub fn render(markdown: &str, width: usize) -> Vec<String> {
    let palette = theme::palette();
    let (marker_style, muted) = (palette.marker, palette.muted);
    let width = width.max(8);
    let source: Vec<&str> = markdown.lines().collect();
    let mut output = Vec::new();
    let mut index = 0usize;
    let mut fence: Option<Fence> = None;
    // Content column of the latest list item; indented lines that follow it
    // continue the item instead of starting a flush-left paragraph.
    let mut list_content: Option<usize> = None;

    while index < source.len() {
        let line = source[index];
        let trimmed = line.trim_start();
        if let Some(open) = &fence {
            if open.closes(trimmed) {
                fence = None;
            } else {
                render_code_line(&open.language, open.strip_indent(line), width, &mut output);
            }
            index += 1;
            continue;
        }
        if let Some(open) = Fence::open(line) {
            fence = Some(open);
            index += 1;
            continue;
        }

        if index + 1 < source.len() && is_table_row(line) && is_table_separator(source[index + 1]) {
            let mut rows = vec![parse_cells(line)];
            index += 2;
            while index < source.len() && is_table_row(source[index]) {
                rows.push(parse_cells(source[index]));
                index += 1;
            }
            render_table(&rows, width, &mut output);
            list_content = None;
            continue;
        }

        let indent = leading_columns(line).min(MAX_INDENT);
        if trimmed.is_empty() {
            output.push(String::new());
        } else if let Some((level, content)) = heading(trimmed) {
            let style = match level {
                1 => palette.heading_major,
                2 => palette.heading_minor,
                _ => HEADING,
            };
            let styled = format!("{style}{}{RESET}", render_inline_styled(content, style));
            push_wrapped("", "", &styled, width, &mut output);
            list_content = None;
        } else if is_rule(trimmed) {
            output.push(format!("{muted}{}{RESET}", "─".repeat(width)));
            list_content = None;
        } else if let Some((depth, content)) = blockquote(trimmed) {
            let bars = format!("{marker_style}{}{RESET}", "│ ".repeat(depth));
            let blank = " ".repeat(depth * 2);
            let styled = format!("{muted}{}{RESET}", render_inline_styled(content, muted));
            push_wrapped(&bars, &blank, &styled, width, &mut output);
        } else if let Some((marker, content)) = list_item(trimmed, indent) {
            let pad = " ".repeat(indent);
            let prefix = format!("{pad}{marker_style}{marker}{RESET} ");
            let continuation = " ".repeat(indent + visible_width(&marker) + 1);
            list_content = Some(continuation.len());
            push_wrapped(
                &prefix,
                &continuation,
                &render_inline(content),
                width,
                &mut output,
            );
        } else if let Some(column) = list_content.filter(|_| indent > 0) {
            let pad = " ".repeat(column);
            push_wrapped(&pad, &pad, &render_inline(trimmed), width, &mut output);
        } else {
            push_wrapped("", "", &render_inline(trimmed), width, &mut output);
            list_content = None;
        }
        index += 1;
    }

    if output.is_empty() {
        output.push(String::new());
    }
    output
}

/// An open fenced code block: its marker, minimum length, opening indent,
/// and info-string language.
struct Fence {
    marker: char,
    length: usize,
    indent: usize,
    language: String,
}

impl Fence {
    fn open(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let length = trimmed.chars().take_while(|c| *c == marker).count();
        if length < 3 {
            return None;
        }
        let info = trimmed[length..].trim();
        // Backtick fences cannot carry backticks in their info string, so
        // inline code that starts a line is not mistaken for a fence.
        if marker == '`' && info.contains('`') {
            return None;
        }
        Some(Self {
            marker,
            length,
            indent: line.len() - trimmed.len(),
            language: info.split_whitespace().next().unwrap_or("").to_string(),
        })
    }

    fn closes(&self, trimmed: &str) -> bool {
        let run = trimmed.chars().take_while(|c| *c == self.marker).count();
        run >= self.length && trimmed[run..].trim().is_empty()
    }

    /// Fences nested in list items are indented; drop that shared indent so
    /// the code keeps its own alignment.
    fn strip_indent<'a>(&self, line: &'a str) -> &'a str {
        let removable = line
            .bytes()
            .take(self.indent)
            .take_while(|byte| *byte == b' ')
            .count();
        &line[removable..]
    }
}

fn leading_columns(line: &str) -> usize {
    line.chars()
        .take_while(|character| character.is_whitespace())
        .map(|character| if character == '\t' { 4 } else { 1 })
        .sum()
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    // Closing hashes are decoration: `## Title ##`.
    let content = rest.trim();
    let content = content
        .trim_end_matches('#')
        .strip_suffix(' ')
        .map_or(content, str::trim_end);
    Some((level, content))
}

fn blockquote(line: &str) -> Option<(usize, &str)> {
    let mut depth = 0;
    let mut rest = line;
    while let Some(after) = rest.strip_prefix('>') {
        depth += 1;
        rest = after.trim_start();
    }
    (depth > 0).then_some((depth, rest))
}

/// Bullet or ordered list item, with task-list checkboxes. Nested bullets
/// switch glyphs so depth stays readable without the source indentation.
fn list_item(line: &str, indent: usize) -> Option<(String, &str)> {
    if let Some(content) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
    {
        let bullet = if indent == 0 { "•" } else { "◦" };
        if let Some(rest) = content.strip_prefix("[ ] ") {
            return Some(("☐".into(), rest));
        }
        if let Some(rest) = content
            .strip_prefix("[x] ")
            .or_else(|| content.strip_prefix("[X] "))
        {
            return Some(("☑".into(), rest));
        }
        return Some((bullet.into(), content));
    }
    let digits = line
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &line[digits..];
    let delimiter = rest.chars().next().filter(|c| matches!(c, '.' | ')'))?;
    let content = rest[1..].strip_prefix(' ')?;
    Some((format!("{}{delimiter}", &line[..digits]), content))
}

fn is_rule(line: &str) -> bool {
    let compact: String = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let Some(first) = compact.chars().next() else {
        return false;
    };
    compact.len() >= 3
        && matches!(first, '-' | '*' | '_')
        && compact.chars().all(|character| character == first)
}

fn render_code_line(language: &str, line: &str, width: usize, output: &mut Vec<String>) {
    let code_background = theme::palette().code_background;
    let plain = sanitize(line).replace('\t', "    ");
    let chunks = split_chars(&plain, width.max(1));
    if chunks.is_empty() {
        output.push(format!(
            "{code_background}{path}{RESET}",
            path = " ".repeat(width)
        ));
    } else {
        for chunk in chunks {
            let highlighted = highlight::render_line(language, &chunk);
            let padded = fit_width(&highlighted, width);
            output.push(format!(
                "{code_background}{}{RESET}",
                padded.replace(RESET, &format!("{RESET}{code_background}"))
            ));
        }
    }
}

fn render_inline(text: &str) -> String {
    render_inline_styled(text, "")
}

/// Renders inline markdown inside a block whose own style is `base`. Each
/// span ends with a reset followed by `base`, so a bold word or code span in
/// a heading or quote returns to the surrounding style instead of plain text.
fn render_inline_styled(text: &str, base: &str) -> String {
    let chars: Vec<char> = sanitize(text).chars().collect();
    let mut output = String::new();
    render_spans(&chars, base, &mut output);
    output
}

/// Emphasis delimiters, longest first so `***` is not read as `**` + `*`.
const EMPHASIS: [(&str, &str); 7] = [
    ("***", "\x1b[1;3m"),
    ("___", "\x1b[1;3m"),
    ("**", BOLD),
    ("__", BOLD),
    ("~~", STRIKE),
    ("*", ITALIC),
    ("_", ITALIC),
];

fn render_spans(chars: &[char], base: &str, output: &mut String) {
    let code = code_spans(chars);
    let closers = EMPHASIS.map(|(marker, _)| emphasis_closers(chars, marker, &code));
    let mut labels = DelimiterSearch::default();
    let mut urls = DelimiterSearch::default();
    let mut code_index = 0usize;
    let mut index = 0usize;
    'scan: while index < chars.len() {
        while code.get(code_index).is_some_and(|span| span.end <= index) {
            code_index += 1;
        }
        if let Some(span) = code.get(code_index).filter(|span| span.start == index) {
            output.push_str(theme::palette().inline_code);
            output.extend(trim_code(&chars[span.content.clone()]));
            output.push_str(RESET);
            output.push_str(base);
            index = span.end;
            continue;
        }
        let character = chars[index];
        if character == '\\'
            && let Some(next) = chars.get(index + 1).filter(|c| c.is_ascii_punctuation())
        {
            output.push(*next);
            index += 2;
            continue;
        }
        if character == '['
            && let Some(close_label) = labels.find(chars, index + 1, ']')
        {
            if chars.get(close_label + 1) == Some(&'(')
                && let Some(close_url) = urls.find(chars, close_label + 2, ')')
            {
                let underlined = format!("{base}{UNDERLINE}");
                output.push_str(UNDERLINE);
                render_spans(&chars[index + 1..close_label], &underlined, output);
                output.push_str(RESET);
                output.push_str(base);
                output.push_str(" \x1b[2m(");
                output.extend(&chars[close_label + 2..close_url]);
                output.push(')');
                output.push_str(RESET);
                output.push_str(base);
                index = close_url + 1;
                continue;
            }
            output.push(character);
            index += 1;
            continue;
        }
        if character == '<'
            && let Some(close) = chars[index + 1..]
                .iter()
                .take(MAX_AUTOLINK_CHARS)
                .position(|c| *c == '>')
        {
            let inner = &chars[index + 1..index + 1 + close];
            let link = inner.iter().collect::<String>();
            if (link.starts_with("http://") || link.starts_with("https://"))
                && !inner.iter().any(|c| c.is_whitespace())
            {
                output.push_str(UNDERLINE);
                output.push_str(&link);
                output.push_str(RESET);
                output.push_str(base);
                index += close + 2;
                continue;
            }
        }
        for (slot, (marker, style)) in EMPHASIS.iter().enumerate() {
            let length = marker.chars().count();
            if !opens_emphasis(chars, index, marker) {
                continue;
            }
            let first_closer =
                closers[slot].partition_point(|position| *position < index + length + 1);
            let Some(&close) = closers[slot].get(first_closer) else {
                continue;
            };
            let nested = format!("{base}{style}");
            output.push_str(style);
            render_spans(&chars[index + length..close], &nested, output);
            output.push_str(RESET);
            output.push_str(base);
            index = close + length;
            continue 'scan;
        }
        output.push(character);
        index += 1;
    }
}

struct CodeSpan {
    start: usize,
    end: usize,
    content: std::ops::Range<usize>,
}

/// Backtick runs pair with the next run of the same length, as in CommonMark.
fn code_spans(chars: &[char]) -> Vec<CodeSpan> {
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '`' && (index == 0 || chars[index - 1] != '\\') {
            let length = chars[index..].iter().take_while(|c| **c == '`').count();
            runs.push((index, length));
            index += length;
        } else {
            index += 1;
        }
    }
    let mut spans = Vec::new();
    let mut run = 0usize;
    while run < runs.len() {
        let (start, length) = runs[run];
        match runs[run + 1..]
            .iter()
            .position(|(_, other)| *other == length)
        {
            Some(offset) => {
                let (close, _) = runs[run + 1 + offset];
                spans.push(CodeSpan {
                    start,
                    end: close + length,
                    content: start + length..close,
                });
                run += offset + 2;
            }
            None => run += 1,
        }
    }
    spans
}

/// Strips one space from both ends when both are present, so `` ` `x` ` ``
/// can show backticks at its edges.
fn trim_code(content: &[char]) -> &[char] {
    if content.len() >= 2
        && content[0] == ' '
        && content[content.len() - 1] == ' '
        && content.iter().any(|c| *c != ' ')
    {
        &content[1..content.len() - 1]
    } else {
        content
    }
}

/// Opening delimiters must touch the text they wrap. Underscores inside
/// words (`snake_case`) and lone operators (`2 * 3`) stay literal.
fn opens_emphasis(chars: &[char], index: usize, marker: &str) -> bool {
    if !starts_with(chars, index, marker) {
        return false;
    }
    let length = marker.chars().count();
    let delimiter = marker.chars().next().unwrap_or('*');
    let before = index.checked_sub(1).map(|before| chars[before]);
    let after = chars.get(index + length).copied();
    if before == Some(delimiter) {
        return false;
    }
    let Some(after) = after else {
        return false;
    };
    if after.is_whitespace() || (length < 3 && after == delimiter && marker != "~~") {
        return false;
    }
    !(delimiter == '_' && before.is_some_and(char::is_alphanumeric))
}

fn emphasis_closers(chars: &[char], marker: &str, code: &[CodeSpan]) -> Vec<usize> {
    let length = marker.chars().count();
    let delimiter = marker.chars().next().unwrap_or('*');
    let mut positions = Vec::new();
    let mut span = 0usize;
    for index in 1..chars.len() {
        while code.get(span).is_some_and(|code| code.end <= index) {
            span += 1;
        }
        if code
            .get(span)
            .is_some_and(|code| code.start <= index && index < code.end)
        {
            continue;
        }
        if !starts_with(chars, index, marker) {
            continue;
        }
        let before = chars[index - 1];
        let after = chars.get(index + length).copied();
        if before.is_whitespace() || before == delimiter || after == Some(delimiter) {
            continue;
        }
        if delimiter == '_' && after.is_some_and(char::is_alphanumeric) {
            continue;
        }
        positions.push(index);
    }
    positions
}

/// Searches only forwards. Failed link parses can revisit the same closing
/// delimiter, but never rescan the suffix for every opening bracket.
#[derive(Default)]
struct DelimiterSearch {
    cursor: usize,
}

impl DelimiterSearch {
    fn find(&mut self, chars: &[char], start: usize, delimiter: char) -> Option<usize> {
        self.cursor = self.cursor.max(start);
        while self.cursor < chars.len() {
            if chars[self.cursor] == delimiter {
                return Some(self.cursor);
            }
            self.cursor += 1;
        }
        None
    }
}

fn starts_with(chars: &[char], index: usize, marker: &str) -> bool {
    let Some(suffix) = chars.get(index..) else {
        return false;
    };
    let mut suffix = suffix.iter().copied();
    marker
        .chars()
        .all(|expected| suffix.next() == Some(expected))
}

fn push_wrapped(
    prefix: &str,
    continuation: &str,
    content: &str,
    width: usize,
    output: &mut Vec<String>,
) {
    let first_width = width.saturating_sub(visible_width(prefix)).max(1);
    let continuation_width = width.saturating_sub(visible_width(continuation)).max(1);
    let mut wrapped = wrap_ansi(content, first_width);
    if wrapped.is_empty() {
        output.push(prefix.to_string());
        return;
    }
    output.push(format!("{prefix}{}", wrapped.remove(0)));
    if continuation_width == first_width {
        output.extend(
            wrapped
                .into_iter()
                .map(|line| format!("{continuation}{line}")),
        );
    } else {
        let remainder = wrapped.join(" ");
        output.extend(
            wrap_ansi(&remainder, continuation_width)
                .into_iter()
                .map(|line| format!("{continuation}{line}")),
        );
    }
}

fn is_table_row(line: &str) -> bool {
    line.contains('|') && parse_cells(line).len() >= 2
}

fn is_table_separator(line: &str) -> bool {
    let cells = parse_cells(line);
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let core = cell.trim().trim_matches(':').trim();
            core.len() >= 3 && core.chars().all(|character| character == '-')
        })
}

fn parse_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn is_row_separator(row: &[String]) -> bool {
    !row.is_empty()
        && row.iter().all(|cell| {
            let core = cell.trim().trim_matches(':').trim();
            !core.is_empty() && core.chars().all(|character| character == '-')
        })
}

fn render_table(rows: &[Vec<String>], width: usize, output: &mut Vec<String>) {
    let muted = theme::palette().muted;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    // Borders plus one space of padding on each side of every cell.
    let available = width.saturating_sub(columns + 1 + columns * 2);
    if columns == 0 || available < columns {
        for row in rows {
            push_wrapped("", "", &render_inline(&row.join(" | ")), width, output);
        }
        return;
    }
    let mut widths = vec![1usize; columns];
    for row in rows {
        if is_row_separator(row) {
            continue;
        }
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(visible_width(&render_inline(cell)));
        }
    }
    while widths.iter().sum::<usize>() > available {
        let Some((largest, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, size)| **size > 1)
            .max_by_key(|(_, size)| **size)
        else {
            break;
        };
        widths[largest] -= 1;
    }

    output.push(table_border('┌', '┬', '┐', &widths));
    for (row_index, row) in rows.iter().enumerate() {
        if row_index > 0 && is_row_separator(row) {
            output.push(table_border('├', '┼', '┤', &widths));
            continue;
        }
        let cells = widths
            .iter()
            .copied()
            .enumerate()
            .map(|(column, cell_width)| {
                let cell = row.get(column).map_or("", String::as_str);
                let rendered = if row_index == 0 {
                    format!("{BOLD}{}{RESET}", render_inline_styled(cell, BOLD))
                } else {
                    render_inline(cell)
                };
                wrap_ansi(&rendered, cell_width)
            })
            .collect::<Vec<_>>();
        let row_height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for physical_row in 0..row_height {
            let mut line = format!("{muted}│{RESET}");
            for (cell, cell_width) in cells.iter().zip(widths.iter().copied()) {
                let content = cell.get(physical_row).map_or("", String::as_str);
                line.push(' ');
                line.push_str(&fit_width(content, cell_width));
                line.push_str(&format!(" {muted}│{RESET}"));
            }
            output.push(line);
        }
        if row_index == 0 && rows.len() > 1 {
            output.push(table_border('├', '┼', '┤', &widths));
        }
    }
    output.push(table_border('└', '┴', '┘', &widths));
}

fn table_border(left: char, join: char, right: char, widths: &[usize]) -> String {
    let mut line = String::from(theme::palette().muted);
    line.push(left);
    for (index, width) in widths.iter().copied().enumerate() {
        line.push_str(&"─".repeat(width + 2));
        line.push(if index + 1 == widths.len() {
            right
        } else {
            join
        });
    }
    line.push_str(RESET);
    line
}

fn terminal_character_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

/// One step through styled text: a whole CSI escape sequence or a single
/// character. An escape byte that starts no sequence is a character.
enum Piece<'a> {
    Escape(&'a str),
    Char(char),
}

/// Splits styled text into escape sequences and characters, each with its
/// byte offset.
fn pieces(text: &str) -> impl Iterator<Item = (usize, Piece<'_>)> {
    let mut index = 0usize;
    std::iter::from_fn(move || {
        let start = index;
        if let Some(end) = ansi_sequence_end(text.as_bytes(), start) {
            index = end;
            return Some((start, Piece::Escape(&text[start..end])));
        }
        let character = text[start..].chars().next()?;
        index += character.len_utf8();
        Some((start, Piece::Char(character)))
    })
}

pub(crate) fn visible_width(text: &str) -> usize {
    pieces(text)
        .map(|(_, piece)| match piece {
            Piece::Escape(_) => 0,
            Piece::Char(character) => terminal_character_width(character),
        })
        .sum()
}

pub(crate) fn fit_width(text: &str, width: usize) -> String {
    let mut line = wrap_ansi_hard(text, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let visible = visible_width(&line);
    if visible < width {
        line.push_str(&" ".repeat(width - visible));
    }
    line
}

/// Fits a composer row using the same grapheme widths as the input editor.
/// Segment the unstyled text so ANSI sequences cannot split a cluster.
/// Transcript fitting retains its existing per-character width convention.
pub(super) fn fit_composer_width(text: &str, width: usize) -> String {
    let plain = strip_ansi(text);
    let mut visible = 0usize;
    let mut remaining_chars = 0usize;
    for grapheme in plain.graphemes(true) {
        let cluster_width = grapheme.width();
        if grapheme.contains('\n') || visible.saturating_add(cluster_width) > width {
            break;
        }
        visible += cluster_width;
        remaining_chars += grapheme.chars().count();
    }

    let mut line = String::new();
    let mut styled = false;
    for (_, piece) in pieces(text) {
        match piece {
            Piece::Escape(sequence) => {
                line.push_str(sequence);
                styled |= sequence.ends_with('m');
            }
            Piece::Char(character) if remaining_chars > 0 => {
                line.push(character);
                remaining_chars -= 1;
            }
            Piece::Char(_) => break,
        }
    }
    if styled && !line.ends_with(RESET) {
        line.push_str(RESET);
    }
    line.push_str(&" ".repeat(width - visible));
    line
}

pub(crate) fn strip_ansi(text: &str) -> String {
    pieces(text)
        .filter_map(|(_, piece)| match piece {
            Piece::Escape(_) => None,
            Piece::Char(character) => Some(character),
        })
        .collect()
}

/// Draws `overlay` over the cells of `text` starting at `column`. Text on
/// both sides keeps its styling; a wide character cut by either edge becomes
/// spaces.
pub(crate) fn overlay_at(text: &str, column: usize, overlay: &str) -> String {
    let end = column + visible_width(overlay);
    let (mut before, mut after, mut replay) = (String::new(), String::new(), String::new());
    let mut cell = 0usize;
    for (_, piece) in pieces(text) {
        let character = match piece {
            Piece::Escape(sequence) => {
                if cell < column {
                    before.push_str(sequence);
                }
                // Styling that starts under the overlay must still apply after it.
                if cell < end {
                    replay.push_str(sequence);
                } else {
                    after.push_str(sequence);
                }
                continue;
            }
            Piece::Char(character) => character,
        };
        let width = terminal_character_width(character);
        if cell + width <= column {
            before.push(character);
        } else if cell < column {
            before.push_str(&" ".repeat(column - cell));
        } else if cell >= end {
            after.push(character);
        } else if cell + width > end {
            after.push_str(&" ".repeat(cell + width - end));
        }
        cell += width;
    }
    if cell < column {
        before.push_str(&" ".repeat(column - cell));
    }
    format!("{before}\x1b[0m{overlay}\x1b[0m{replay}{after}")
}

/// Applies a background color to the final visible character without
/// replacing it or changing the line's terminal width.
pub(crate) fn overlay_last_cell_background(text: &str, background: &str) -> String {
    let mut last_visible = None;
    for (index, piece) in pieces(text) {
        let Piece::Char(character) = piece else {
            continue;
        };
        let end = index + character.len_utf8();
        if terminal_character_width(character) > 0 {
            last_visible = Some((index, end));
        } else if let Some((_, last_end)) = &mut last_visible {
            *last_end = end;
        }
    }

    let Some((start, end)) = last_visible else {
        return text.to_string();
    };
    let mut overlaid = String::with_capacity(text.len() + background.len() + 5);
    overlaid.push_str(&text[..start]);
    overlaid.push_str(background);
    overlaid.push_str(&text[start..end]);
    overlaid.push_str("\x1b[49m");
    overlaid.push_str(&text[end..]);
    overlaid
}

fn ansi_sequence_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&0x1b) || bytes.get(start + 1) != Some(&b'[') {
        return None;
    }
    let mut end = start + 2;
    while end < bytes.len() {
        let byte = bytes[end];
        end += 1;
        if (0x40..=0x7e).contains(&byte) {
            break;
        }
    }
    Some(end)
}

/// A whitespace run that can be replaced with a line break. Styles on each
/// side are stored separately because an ANSI reset may occur inside the run.
struct WrapBreak {
    next_index: usize,
    line_len: usize,
    line_style: String,
    resume_style: String,
}

fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut column = 0usize;
    let mut active_style = String::new();
    let mut last_break: Option<WrapBreak> = None;
    let mut separator_open = false;
    let mut index = 0usize;
    while index < bytes.len() {
        if let Some(end) = ansi_sequence_end(bytes, index) {
            let sequence = &text[index..end];
            line.push_str(sequence);
            track_style(&mut active_style, sequence);
            index = end;
            if separator_open && let Some(break_point) = &mut last_break {
                break_point.next_index = index;
                break_point.resume_style.clone_from(&active_style);
            }
            continue;
        }
        if bytes[index] == b'\n' {
            break_styled_line(&mut lines, &mut line, &active_style);
            column = 0;
            last_break = None;
            separator_open = false;
            index += 1;
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        let next_index = index + character.len_utf8();
        let character_width = terminal_character_width(character);
        if character.is_whitespace() {
            if column == 0 {
                index = next_index;
                continue;
            }
            if !separator_open {
                last_break = Some(WrapBreak {
                    next_index,
                    line_len: line.len(),
                    line_style: active_style.clone(),
                    resume_style: active_style.clone(),
                });
                separator_open = true;
            } else if let Some(break_point) = &mut last_break {
                break_point.next_index = next_index;
                break_point.resume_style.clone_from(&active_style);
            }
        } else {
            separator_open = false;
        }
        if column > 0 && column.saturating_add(character_width) > width {
            if !character.is_whitespace()
                && let Some(break_point) = last_break.take()
            {
                line.truncate(break_point.line_len);
                if !break_point.line_style.is_empty() && !line.ends_with(RESET) {
                    line.push_str(RESET);
                }
                lines.push(line);
                line = break_point.resume_style.clone();
                active_style = break_point.resume_style;
                column = 0;
                separator_open = false;
                index = break_point.next_index;
                continue;
            }
            break_styled_line(&mut lines, &mut line, &active_style);
            column = 0;
            last_break = None;
            separator_open = false;
            if character.is_whitespace() {
                index = next_index;
                continue;
            }
        }
        line.push(character);
        column = column.saturating_add(character_width);
        index = next_index;
    }
    if !line.is_empty() || lines.is_empty() {
        if !active_style.is_empty() && !line.ends_with(RESET) {
            line.push_str(RESET);
        }
        lines.push(line);
    }
    lines
}

pub(super) fn wrap_ansi_hard(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut column = 0usize;
    let mut active_style = String::new();
    for (_, piece) in pieces(text) {
        let character = match piece {
            Piece::Escape(sequence) => {
                line.push_str(sequence);
                track_style(&mut active_style, sequence);
                continue;
            }
            Piece::Char(character) => character,
        };
        if character == '\n' {
            break_styled_line(&mut lines, &mut line, &active_style);
            column = 0;
            continue;
        }
        let character_width = terminal_character_width(character);
        if column > 0 && column.saturating_add(character_width) > width {
            break_styled_line(&mut lines, &mut line, &active_style);
            column = 0;
        }
        line.push(character);
        column = column.saturating_add(character_width);
    }
    if !line.is_empty() || lines.is_empty() {
        if !active_style.is_empty() && !line.ends_with(RESET) {
            line.push_str(RESET);
        }
        lines.push(line);
    }
    lines
}

/// Remembers the style an SGR sequence leaves active, so wrapped lines can
/// reopen it.
fn track_style(active_style: &mut String, sequence: &str) {
    if sequence == RESET {
        active_style.clear();
    } else if sequence.ends_with('m') {
        *active_style = sequence.to_string();
    }
}

/// Ends `line`, closing its style, and starts the next line in that style.
fn break_styled_line(lines: &mut Vec<String>, line: &mut String, active_style: &str) {
    if !active_style.is_empty() {
        line.push_str(RESET);
    }
    lines.push(std::mem::replace(line, active_style.to_string()));
}

pub(super) fn split_chars(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_width = 0usize;
    for (index, character) in text.char_indices() {
        let character_width = terminal_character_width(character);
        if chunk_width > 0 && chunk_width.saturating_add(character_width) > width {
            chunks.push(text[start..index].to_string());
            start = index;
            chunk_width = 0;
        }
        chunk_width = chunk_width.saturating_add(character_width);
    }
    if start < text.len() {
        chunks.push(text[start..].to_string());
    }
    chunks
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\t' || !character.is_control() {
                character
            } else {
                '�'
            }
        })
        .collect()
}

fn terminal_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut cursor = 0usize;
    for (_, piece) in pieces(text) {
        let character = match piece {
            Piece::Escape(sequence) => {
                if sequence.ends_with('K') {
                    erase_terminal_line(&mut line, cursor, sequence);
                }
                continue;
            }
            Piece::Char(character) => character,
        };
        match character {
            '\n' => {
                lines.push(line.iter().collect());
                line.clear();
                cursor = 0;
            }
            '\r' => cursor = 0,
            '\u{8}' => cursor = cursor.saturating_sub(1),
            character => {
                let character = if character == '\t' || !character.is_control() {
                    character
                } else {
                    char::REPLACEMENT_CHARACTER
                };
                if cursor < line.len() {
                    line[cursor] = character;
                } else {
                    line.resize(cursor, ' ');
                    line.push(character);
                }
                cursor = cursor.saturating_add(1);
            }
        }
    }
    lines.push(line.iter().collect());
    lines
}

fn erase_terminal_line(line: &mut Vec<char>, cursor: usize, sequence: &str) {
    let mode = sequence
        .strip_prefix("\x1b[")
        .and_then(|parameters| parameters.strip_suffix('K'));
    match mode {
        Some("") | Some("0") => line.truncate(cursor),
        Some("1") => {
            for character in line.iter_mut().take(cursor.saturating_add(1)) {
                *character = ' ';
            }
        }
        Some("2") => line.clear(),
        _ => {}
    }
}

/// Renders untrusted plain text as hard-wrapped terminal lines. Markdown and
/// terminal cursor updates are reduced to the text visible on each line.
pub(crate) fn plain_lines(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in terminal_lines(text) {
        lines.extend(wrap_ansi_hard(&line, width.max(1)));
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn wrapped_plain_lines(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for line in terminal_lines(text) {
        lines.extend(wrap_ansi(&line, width.max(1)));
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

pub(crate) fn wrapped_plain_prefixed_lines(
    prefix: &str,
    continuation: &str,
    text: &str,
    width: usize,
) -> Vec<String> {
    let text = terminal_lines(text).join(" ");
    let mut lines = Vec::new();
    push_wrapped(prefix, continuation, &text, width.max(1), &mut lines);
    lines
}

#[cfg(test)]
mod tests {
    use super::super::theme::palette;

    #[test]
    fn overlay_keeps_styling_on_both_sides() {
        let line = "\x1b[31mred text\x1b[0m and \x1b[32mgreen tail\x1b[0m";
        let overlaid = overlay_at(line, 4, "[XX]");
        assert_eq!(strip_ansi(&overlaid), "red [XX] and green tail");
        assert_eq!(visible_width(&overlaid), visible_width(line));
        // The red run under the overlay continues after it.
        let after = overlaid.split("[XX]").nth(1).expect("after overlay");
        assert!(after.starts_with("\x1b[0m\x1b[31m"));
        // A wide character cut by the edge becomes padding.
        assert_eq!(strip_ansi(&overlay_at("ab界cd", 3, "X")), "ab Xcd");
        assert_eq!(strip_ansi(&overlay_at("ab", 4, "X")), "ab  X");
    }

    use super::*;

    #[test]
    fn malformed_links_preserve_literal_text_and_later_valid_links() {
        for text in [
            "[".repeat(40_000),
            format!("{}](unfinished", "[".repeat(20_000)),
        ] {
            assert_eq!(render_inline(&text), text);
        }
        assert_eq!(
            render_inline("[broken] then [ok](url)"),
            "[broken] then \x1b[4mok\x1b[0m \x1b[2m(url)\x1b[0m"
        );
        assert_eq!(
            render_inline("[[label](url)"),
            "\x1b[4m[label\x1b[0m \x1b[2m(url)\x1b[0m"
        );
    }

    #[test]
    fn renders_inline_styles_and_wraps_by_visible_width() {
        let lines = render("hello **bold** and `code`", 12);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| visible_width(line) <= 12));
        let rendered = lines.join("");
        assert!(rendered.contains("\x1b[1mbold\x1b[0m"));
        assert!(rendered.contains(&format!("{}code{RESET}", palette().inline_code)));
        assert!(!rendered.contains("\x1b[30;47m"));
        assert!(!rendered.contains("\x1b[7m"));
    }

    #[test]
    fn prose_wraps_at_word_boundaries_and_reflows_for_width() {
        let text = "hello wonderful world";
        let narrow = render(text, 12)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();
        let wide = render(text, 20)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(narrow, ["hello", "wonderful", "world"]);
        assert_eq!(wide, ["hello wonderful", "world"]);
        assert_eq!(narrow.join(" "), text);
        assert_eq!(wide.join(" "), text);
    }

    #[test]
    fn plain_terminal_text_preserves_lines_and_replays_carriage_returns() {
        let lines = plain_lines(
            "Compiling alpha\r\nBuilding a long stale line\r\x1b[2KBuilding [==> ] 2/2\nFinished",
            80,
        );

        assert_eq!(
            lines,
            ["Compiling alpha", "Building [==> ] 2/2", "Finished"]
        );
        assert!(!lines.join("\n").contains('�'));
    }

    #[test]
    fn styled_words_keep_their_style_after_wrapping() {
        let lines = render("before **bold words** after", 10);
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();

        assert_eq!(plain, ["before", "bold words", "after"]);
        assert!(lines[1].starts_with("\x1b[1m"), "{:?}", lines[1]);
    }

    #[test]
    fn oversized_words_and_fixed_width_fields_still_hard_wrap() {
        let lines = render("abcdefghijkl end", 8)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(lines, ["abcdefgh", "ijkl end"]);
        assert_eq!(strip_ansi(&fit_width("alpha beta", 7)), "alpha b");
    }

    #[test]
    fn background_overlay_preserves_the_last_character_and_width() {
        let line = fit_width("\x1b[3mabcdefgh\x1b[0m", 8);
        let overlaid = overlay_last_cell_background(&line, "\x1b[48;2;1;2;3m");

        assert_eq!(strip_ansi(&overlaid), "abcdefgh");
        assert_eq!(visible_width(&overlaid), 8);
        assert!(overlaid.contains("\x1b[48;2;1;2;3mh\x1b[49m"));
    }

    #[test]
    fn list_continuations_wrap_whole_words_under_the_content() {
        let lines = render("- alpha wonderful omega", 12)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(lines, ["• alpha", "  wonderful", "  omega"]);
    }

    #[test]
    fn wide_glyphs_use_their_terminal_cell_width() {
        assert_eq!(visible_width("a✅b"), 4);

        let lines = render("abcdefg✅hi", 8);

        assert!(
            lines.iter().all(|line| visible_width(line) <= 8),
            "{:?}",
            lines
                .iter()
                .map(|line| (strip_ansi(line), visible_width(line)))
                .collect::<Vec<_>>()
        );
        assert_eq!(strip_ansi(&lines.join("")), "abcdefg✅hi");
    }

    #[test]
    fn composer_fitting_preserves_clusters_and_ansi_styles() {
        let family = "👨\u{200d}👩\u{200d}👧";
        for text in [
            format!("\x1b[31m{family}abc\x1b[0m"),
            "👨\u{200d}\x1b[31m👩\u{200d}👧abc\x1b[0m".into(),
        ] {
            let fitted = fit_composer_width(&text, 4);
            assert_eq!(strip_ansi(&fitted), format!("{family}ab"));
            assert!(fitted.contains("\x1b[31m"));
            assert!(fitted.ends_with(RESET));
            assert_eq!(strip_ansi(&fit_composer_width(&text, 1)), " ");
        }
        assert_eq!(fit_composer_width("e\u{301}", 3), "e\u{301}  ");
        assert_eq!(strip_ansi(&fit_composer_width("abc", 0)), "");
        assert_eq!(fit_composer_width("a\nb", 3), "a  ");
    }

    #[test]
    fn variation_selectors_preserve_the_base_character_width() {
        assert_eq!(visible_width("a⚠️b"), 3);

        let lines = wrap_ansi("abcdefg⚠️hi", 8);

        assert!(
            lines.iter().all(|line| visible_width(line) <= 8),
            "{:?}",
            lines
                .iter()
                .map(|line| (strip_ansi(line), visible_width(line)))
                .collect::<Vec<_>>()
        );
        assert_eq!(strip_ansi(&lines.join("")), "abcdefg⚠️hi");
    }

    #[test]
    fn renders_box_drawing_table() {
        let lines = render("| Name | Value |\n| --- | --- |\n| yawl | small |", 40);
        assert!(strip_ansi(&lines[0]).starts_with('┌'));
        assert!(lines.iter().any(|line| strip_ansi(line).contains('┼')));
        assert!(
            lines
                .last()
                .is_some_and(|line| strip_ansi(line).starts_with('└'))
        );
    }

    #[test]
    fn renders_table_with_interior_separator_rows() {
        let md = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| --- | --- |\n| 3 | 4 |";
        let lines = render(md, 40);
        assert_eq!(lines.iter().filter(|l| l.contains('┼')).count(), 2);
    }

    #[test]
    fn table_borders_align_with_emoji_presentation_sequences() {
        fn terminal_fixture_width(line: &str) -> usize {
            strip_ansi(line)
                .chars()
                .map(|character| unicode_width::UnicodeWidthChar::width(character).unwrap_or(0))
                .sum()
        }

        let lines = render(
            "| Interface | Status |\n| --- | --- |\n| en0 | ✅ Active |\n| en4 | ⚠️ No IP assigned |",
            20,
        );
        let border_width = terminal_fixture_width(&lines[0]);

        assert!(
            lines
                .iter()
                .all(|line| terminal_fixture_width(line) == border_width),
            "{:?}",
            lines
                .iter()
                .map(|line| (strip_ansi(line), terminal_fixture_width(line)))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn table_cells_wrap_whole_words_instead_of_truncating_them() {
        let lines = render(
            "| Item | Description |\n| --- | --- |\n| one | hello wonderful world |",
            20,
        );
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();

        assert!(plain.iter().any(|line| line.contains("│ one  │ hello")));
        assert!(plain.iter().any(|line| line.contains("│      │ wonderful")));
        assert!(plain.iter().any(|line| line.contains("│      │ world")));
        assert!(plain.iter().all(|line| visible_width(line) == 20));
    }

    #[test]
    fn highlights_fenced_rust() {
        let lines = render("```rust\nfn main() {}\n```", 40);
        assert!(
            lines
                .join("\n")
                .contains(&format!("{}fn{RESET}", theme::palette().keyword))
        );
    }

    #[test]
    fn fenced_code_renders_without_copy_chrome() {
        let lines = render(
            "```bash\nhf download foo/bar model.gguf \\\n  --local-dir ~/models\n\n# comment\n```",
            80,
        );
        // Drag-select strips ANSI and trims line ends, so trimmed plain text
        // is what lands on the clipboard.
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line).trim_end().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            plain,
            [
                "hf download foo/bar model.gguf \\",
                "  --local-dir ~/models",
                "",
                "# comment",
            ]
        );
        for line in &plain {
            assert!(!line.contains('│'));
            assert!(!line.contains("┌─"));
            assert!(!line.contains("└─"));
        }
    }

    #[test]
    fn fenced_code_background_differs_from_user_and_tool_cards() {
        let lines = render("```bash\necho hi\n```", 40);
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(
                line.contains(palette().code_background),
                "code lines need the shaded background: {line:?}"
            );
            assert_eq!(visible_width(line), 40);
        }
        // Warm dark gray must stay distinct from the cool user-prompt panel
        // and the green/red/purple tool cards.
        let palette = palette();
        for other in [
            palette.user_background,
            palette.card_success,
            palette.card_error,
            palette.card_skill,
        ] {
            assert_ne!(palette.code_background, other);
        }
    }

    #[test]
    fn unclosed_fence_renders_without_copy_chrome() {
        let lines = render("```bash\necho hi", 80);
        let plain = lines
            .iter()
            .map(|line| strip_ansi(line).trim_end().to_string())
            .collect::<Vec<_>>();
        assert_eq!(plain, ["echo hi"]);
    }

    #[test]
    fn strips_untrusted_terminal_escapes() {
        let lines = render("unsafe \x1b[2J text", 40);
        assert!(!lines.join("").contains("\x1b[2J"));
    }

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| strip_ansi(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn headings_drop_markers_and_keep_their_style_around_inline_spans() {
        let lines = render("# Target\n## Notes ##\n### Deep `code` **bold** tail", 60);
        assert_eq!(plain(&lines), ["Target", "Notes", "Deep code bold tail"]);
        assert!(
            lines[0].starts_with(palette().heading_major),
            "{:?}",
            lines[0]
        );
        assert!(
            lines[1].starts_with(palette().heading_minor),
            "{:?}",
            lines[1]
        );
        assert!(!lines.join("").contains("\x1b[1;34m"));
        // After the code and bold spans the heading style comes back.
        assert!(
            lines[2].contains(&format!("{RESET}{HEADING} tail")),
            "{:?}",
            lines[2]
        );
        assert_eq!(plain(&render("#hashtag", 40)), ["#hashtag"]);
    }

    #[test]
    fn emphasis_needs_flanking_delimiters() {
        for literal in [
            "snake_case_name and other_name",
            "2 * 3 * 4",
            "a ** b ** c",
            "foo__bar__baz",
        ] {
            assert_eq!(render_inline(literal), literal, "{literal}");
        }
        assert_eq!(render_inline("*soft*"), format!("{ITALIC}soft{RESET}"));
        assert_eq!(render_inline("_soft_"), format!("{ITALIC}soft{RESET}"));
        assert_eq!(render_inline("***both***"), format!("\x1b[1;3mboth{RESET}"));
        assert_eq!(render_inline("~~gone~~"), format!("{STRIKE}gone{RESET}"));
        assert_eq!(render_inline(r"\*literal\*"), "*literal*");
        assert_eq!(
            render_inline("**bold `code` end**"),
            format!(
                "{BOLD}bold {}code{RESET}{BOLD} end{RESET}",
                palette().inline_code
            )
        );
    }

    #[test]
    fn code_spans_pair_matching_runs_and_shield_their_contents() {
        assert_eq!(
            render_inline("``a ` b``"),
            format!("{}a ` b{RESET}", palette().inline_code)
        );
        assert_eq!(
            render_inline("*see `a*b` here*"),
            format!(
                "{ITALIC}see {}a*b{RESET}{ITALIC} here{RESET}",
                palette().inline_code
            )
        );
        assert_eq!(render_inline("unpaired ` tick"), "unpaired ` tick");
    }

    #[test]
    fn nested_lists_keep_depth_tasks_and_continuations() {
        let lines = render(
            "- top\n  - nested\n    more nested text\n1) first\n- [ ] todo\n- [x] done\n\nAfter",
            40,
        );
        assert_eq!(
            plain(&lines),
            [
                "• top",
                "  ◦ nested",
                "    more nested text",
                "1) first",
                "☐ todo",
                "☑ done",
                "",
                "After",
            ]
        );
    }

    #[test]
    fn rules_quotes_and_tilde_fences_render_as_blocks() {
        let lines = render(
            "* * *\n> quoted **bold**\n>> deeper\n~~~python\nx = 1\n~~~\n- item\n  ```sh\n  echo hi\n  ```",
            30,
        );
        let plain = plain(&lines);
        assert_eq!(plain[0], "─".repeat(30));
        assert_eq!(plain[1], "│ quoted bold");
        assert_eq!(plain[2], "│ │ deeper");
        assert_eq!(plain[3], "x = 1");
        assert!(lines[3].contains(palette().code_background));
        assert_eq!(plain[4], "• item");
        assert_eq!(plain[5], "echo hi", "fence indentation is stripped");
        assert_eq!(plain.len(), 6);
    }

    #[test]
    fn inline_rendering_stays_linear_on_delimiter_floods() {
        let started = std::time::Instant::now();
        for text in [
            "a_".repeat(20_000),
            "* ".repeat(20_000),
            format!("`{}", "x`y".repeat(10_000)),
            "<".repeat(40_000),
        ] {
            let _ = render(&text, 120);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn panel_headings_keep_the_panel_background() {
        let lines = super::super::render::render_user_panel("# Target\nbody **bold** text", 40);
        for line in &lines {
            // Every reset must be followed by the panel style again.
            for (index, _) in line.match_indices(RESET) {
                let after = &line[index + RESET.len()..];
                assert!(
                    after.is_empty() || after.starts_with(palette().user_background),
                    "{line:?}"
                );
            }
        }
        assert_eq!(strip_ansi(&lines[1]).trim(), "Target");
    }

    #[test]
    fn lone_escape_bytes_are_plain_characters() {
        assert_eq!(visible_width("a\x1bb"), 2);
        assert_eq!(strip_ansi("a\x1bb\x1b[31mc\x1b[0m"), "a\x1bbc");
        assert_eq!(fit_width("\x1b", 3), "\x1b   ");
    }
}
