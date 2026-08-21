//! Terminal markdown renderer with ANSI styling, wrapping, box-drawing
//! tables, and fenced-code highlighting.

use super::highlight;
use unicode_width::UnicodeWidthChar;

const RESET: &str = "\x1b[0m";
const INLINE_CODE: &str = "\x1b[38;2;155;188;198m";

pub fn render(markdown: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let source: Vec<&str> = markdown.lines().collect();
    let mut output = Vec::new();
    let mut index = 0usize;
    let mut code_language: Option<String> = None;

    while index < source.len() {
        let line = source[index];
        let trimmed = line.trim_start();
        if let Some(language) = &code_language {
            if trimmed.starts_with("```") {
                output.push(fit_width("\x1b[2m└─\x1b[0m", width));
                code_language = None;
            } else {
                render_code_line(language, line, width, &mut output);
            }
            index += 1;
            continue;
        }
        if let Some(info) = trimmed.strip_prefix("```") {
            let language = info.split_whitespace().next().unwrap_or("").to_string();
            let label = if language.is_empty() {
                "\x1b[2m┌─ code\x1b[0m".to_string()
            } else {
                format!("\x1b[2m┌─ {}\x1b[0m", sanitize(&language))
            };
            output.push(fit_width(&label, width));
            code_language = Some(language);
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
            continue;
        }

        if trimmed.is_empty() {
            output.push(String::new());
        } else if let Some((level, content)) = heading(trimmed) {
            let prefix = format!("{} ", "#".repeat(level));
            let styled = format!("\x1b[1;34m{}\x1b[0m", render_inline(content));
            push_wrapped(
                &prefix,
                &" ".repeat(visible_width(&prefix)),
                &styled,
                width,
                &mut output,
            );
        } else if let Some(content) = trimmed
            .strip_prefix("> ")
            .or_else(|| trimmed.strip_prefix('>'))
        {
            push_wrapped(
                "\x1b[36m│\x1b[0m ",
                "  ",
                &render_inline(content.trim_start()),
                width,
                &mut output,
            );
        } else if let Some(content) = unordered_item(trimmed) {
            push_wrapped(
                "\x1b[36m•\x1b[0m ",
                "  ",
                &render_inline(content),
                width,
                &mut output,
            );
        } else if let Some((prefix, content)) = ordered_item(trimmed) {
            let continuation = " ".repeat(visible_width(&prefix));
            push_wrapped(
                &prefix,
                &continuation,
                &render_inline(content),
                width,
                &mut output,
            );
        } else if is_rule(trimmed) {
            output.push(format!("\x1b[2m{}\x1b[0m", "─".repeat(width)));
        } else {
            push_wrapped("", "", &render_inline(trimmed), width, &mut output);
        }
        index += 1;
    }

    if code_language.is_some() {
        output.push(fit_width("\x1b[2m└─\x1b[0m", width));
    }
    if output.is_empty() {
        output.push(String::new());
    }
    output
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if (1..=6).contains(&level) && line.as_bytes().get(level) == Some(&b' ') {
        Some((level, line[level + 1..].trim()))
    } else {
        None
    }
}

fn unordered_item(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
}

fn ordered_item(line: &str) -> Option<(String, &str)> {
    let digits = line
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digits == 0 || !line[digits..].starts_with(". ") {
        return None;
    }
    Some((format!("{} ", &line[..=digits]), &line[digits + 2..]))
}

fn is_rule(line: &str) -> bool {
    let compact: String = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact.len() >= 3
        && compact
            .chars()
            .all(|character| character == '-' || character == '*' || character == '_')
}

fn render_code_line(language: &str, line: &str, width: usize, output: &mut Vec<String>) {
    let available = width.saturating_sub(2).max(1);
    let plain = sanitize(line).replace('\t', "    ");
    let chunks = split_chars(&plain, available);
    if chunks.is_empty() {
        output.push("\x1b[2m│\x1b[0m ".to_string());
    } else {
        for chunk in chunks {
            output.push(format!(
                "\x1b[2m│\x1b[0m {}",
                highlight::render_line(language, &chunk)
            ));
        }
    }
}

fn render_inline(text: &str) -> String {
    let chars: Vec<char> = sanitize(text).chars().collect();
    let mut output = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if starts_with(&chars, index, "**")
            && let Some(end) = find_marker(&chars, index + 2, "**")
        {
            output.push_str("\x1b[1m");
            output.extend(&chars[index + 2..end]);
            output.push_str(RESET);
            index = end + 2;
        } else if chars[index] == '`'
            && let Some(end) = chars[index + 1..]
                .iter()
                .position(|character| *character == '`')
        {
            let end = index + 1 + end;
            output.push_str(INLINE_CODE);
            output.extend(&chars[index + 1..end]);
            output.push_str(RESET);
            index = end + 1;
        } else if chars[index] == '['
            && let Some(close_label) = chars[index + 1..]
                .iter()
                .position(|character| *character == ']')
        {
            let close_label = index + 1 + close_label;
            if chars.get(close_label + 1) == Some(&'(')
                && let Some(close_url) = chars[close_label + 2..]
                    .iter()
                    .position(|character| *character == ')')
            {
                let close_url = close_label + 2 + close_url;
                output.push_str("\x1b[4m");
                output.extend(&chars[index + 1..close_label]);
                output.push_str(RESET);
                output.push_str(" \x1b[2m(");
                output.extend(&chars[close_label + 2..close_url]);
                output.push_str(")\x1b[0m");
                index = close_url + 1;
                continue;
            }
            output.push(chars[index]);
            index += 1;
        } else if matches!(chars[index], '*' | '_')
            && let Some(end) = chars[index + 1..]
                .iter()
                .position(|character| *character == chars[index])
        {
            let end = index + 1 + end;
            output.push_str("\x1b[3m");
            output.extend(&chars[index + 1..end]);
            output.push_str(RESET);
            index = end + 1;
        } else {
            output.push(chars[index]);
            index += 1;
        }
    }
    output
}

fn find_marker(chars: &[char], start: usize, marker: &str) -> Option<usize> {
    (start..chars.len()).find(|index| starts_with(chars, *index, marker))
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

fn render_table(rows: &[Vec<String>], width: usize, output: &mut Vec<String>) {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let available = width.saturating_sub(columns + 1);
    if columns == 0 || available < columns {
        for row in rows {
            push_wrapped("", "", &render_inline(&row.join(" | ")), width, output);
        }
        return;
    }
    let mut widths = vec![1usize; columns];
    for row in rows {
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
        let cells = widths
            .iter()
            .copied()
            .enumerate()
            .map(|(column, cell_width)| {
                let cell = row.get(column).map_or("", String::as_str);
                let rendered = render_inline(cell);
                let rendered = if row_index == 0 {
                    format!("\x1b[1m{rendered}\x1b[0m")
                } else {
                    rendered
                };
                wrap_ansi(&rendered, cell_width)
            })
            .collect::<Vec<_>>();
        let row_height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for physical_row in 0..row_height {
            let mut line = String::from("│");
            for (cell, cell_width) in cells.iter().zip(widths.iter().copied()) {
                let content = cell.get(physical_row).map_or("", String::as_str);
                line.push_str(&fit_width(content, cell_width));
                line.push('│');
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
    let mut line = String::new();
    line.push(left);
    for (index, width) in widths.iter().copied().enumerate() {
        line.push_str(&"─".repeat(width));
        line.push(if index + 1 == widths.len() {
            right
        } else {
            join
        });
    }
    line
}

fn terminal_character_width(character: char) -> usize {
    UnicodeWidthChar::width(character).unwrap_or(0)
}

pub(crate) fn visible_width(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut width = 0;
    let mut index = 0;
    while index < bytes.len() {
        if let Some(end) = ansi_sequence_end(bytes, index) {
            index = end;
        } else {
            let start = index;
            while index < bytes.len() && bytes[index] != 0x1b {
                index += 1;
            }
            width += text[start..index]
                .chars()
                .map(terminal_character_width)
                .sum::<usize>();
        }
    }
    width
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

pub(crate) fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if let Some(end) = ansi_sequence_end(bytes, index) {
            index = end;
        } else {
            let character = text[index..]
                .chars()
                .next()
                .unwrap_or(char::REPLACEMENT_CHARACTER);
            output.push(character);
            index += character.len_utf8();
        }
    }
    output
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
            if sequence == RESET {
                active_style.clear();
            } else if sequence.ends_with('m') {
                active_style = sequence.to_string();
            }
            index = end;
            if separator_open && let Some(break_point) = &mut last_break {
                break_point.next_index = index;
                break_point.resume_style.clone_from(&active_style);
            }
            continue;
        }
        if bytes[index] == b'\n' {
            if !active_style.is_empty() {
                line.push_str(RESET);
            }
            lines.push(line);
            line = active_style.clone();
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
            if !active_style.is_empty() {
                line.push_str(RESET);
            }
            lines.push(line);
            line = active_style.clone();
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

fn wrap_ansi_hard(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut column = 0usize;
    let mut active_style = String::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if let Some(end) = ansi_sequence_end(bytes, index) {
            let start = index;
            index = end;
            let sequence = &text[start..index];
            line.push_str(sequence);
            if sequence == RESET {
                active_style.clear();
            } else if sequence.ends_with('m') {
                active_style = sequence.to_string();
            }
            continue;
        }
        if bytes[index] == b'\n' {
            if !active_style.is_empty() {
                line.push_str(RESET);
            }
            lines.push(line);
            line = active_style.clone();
            column = 0;
            index += 1;
            continue;
        }
        let character = text[index..]
            .chars()
            .next()
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        let character_width = terminal_character_width(character);
        if column > 0 && column.saturating_add(character_width) > width {
            if !active_style.is_empty() {
                line.push_str(RESET);
            }
            lines.push(line);
            line = active_style.clone();
            column = 0;
        }
        line.push(character);
        column = column.saturating_add(character_width);
        index += character.len_utf8();
    }
    if !line.is_empty() || lines.is_empty() {
        if !active_style.is_empty() && !line.ends_with(RESET) {
            line.push_str(RESET);
        }
        lines.push(line);
    }
    lines
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_inline_styles_and_wraps_by_visible_width() {
        let lines = render("hello **bold** and `code`", 12);
        assert!(lines.len() >= 2);
        assert!(lines.iter().all(|line| visible_width(line) <= 12));
        let rendered = lines.join("");
        assert!(rendered.contains("\x1b[1mbold\x1b[0m"));
        assert!(rendered.contains(&format!("{INLINE_CODE}code{RESET}")));
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

        assert!(plain.iter().any(|line| line.contains("│one │hello")));
        assert!(plain.iter().any(|line| line.contains("│    │wonderful")));
        assert!(plain.iter().any(|line| line.contains("│    │world")));
        assert!(plain.iter().all(|line| visible_width(line) == 20));
    }

    #[test]
    fn highlights_fenced_rust() {
        let lines = render("```rust\nfn main() {}\n```", 40);
        assert!(lines.join("\n").contains("\x1b[1;34mfn\x1b[0m"));
    }

    #[test]
    fn strips_untrusted_terminal_escapes() {
        let lines = render("unsafe \x1b[2J text", 40);
        assert!(!lines.join("").contains("\x1b[2J"));
    }
}
