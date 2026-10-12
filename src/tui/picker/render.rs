//! Picker rendering: the item list, the compact detail area, and the
//! focused field.

use super::super::input::Editor;
use super::super::markdown;
use super::super::render::selected_row;
use super::{Picker, PickerEdit, PickerItem};

/// Pickers narrower than this list labels only and show the selected
/// description in a detail area.
const COMPACT_PICKER_COLUMNS: usize = 60;

/// Separator plus two wrapped description lines.
const COMPACT_DETAIL_ROWS: usize = 3;

/// Draws the picker panel centered in `height` rows. Returns the lines and,
/// while a field is focused, the cursor's row and column within them.
pub(in crate::tui) fn render_picker(
    picker: &Picker,
    editor: &Editor,
    selection: &str,
    outline: &str,
    columns: usize,
    height: usize,
) -> (Vec<String>, Option<(usize, usize)>) {
    if height == 0 {
        return (Vec::new(), None);
    }
    // Narrow screens, such as a phone driving `/remote`, drop the margins and
    // move the selected description below the list instead of cutting every
    // row's description short.
    let compact = columns < COMPACT_PICKER_COLUMNS;
    let box_width = if compact {
        columns
    } else {
        columns.saturating_sub(4).max(16).min(columns)
    };
    let inner = box_width.saturating_sub(2);
    let left = " ".repeat(columns.saturating_sub(box_width) / 2);
    let boxed = |content: &str| {
        format!(
            "{left}{outline}│\x1b[0m{}{outline}│\x1b[0m",
            markdown::fit_width(content, inner)
        )
    };
    let separator = format!("{left}{outline}├{}┤\x1b[0m", "─".repeat(inner));
    let mut panel = vec![format!("{left}{outline}┌{}┐\x1b[0m", "─".repeat(inner))];
    panel.push(boxed(&format!(" \x1b[1m{}\x1b[0m", picker.title)));
    panel.push(separator.clone());
    let mut cursor = None;
    let hint = if let Some(PickerEdit::Field(field)) = &picker.editing {
        let (rows, (row, column)) = field.render(inner, outline);
        cursor = Some((panel.len() + row, left.len() + 1 + column));
        panel.extend(rows.iter().map(|row| boxed(row)));
        field.hint(picker.items.is_empty())
    } else {
        render_items(
            picker, editor, selection, compact, inner, height, &boxed, &separator, &mut panel,
        );
        if picker.editing.is_some() {
            "Edit below  Enter save  Esc cancel edit"
        } else {
            &picker.hint
        }
    };
    panel.push(boxed(&format!(" \x1b[2m{hint}\x1b[0m")));
    panel.push(format!("{left}{outline}└{}┘\x1b[0m", "─".repeat(inner)));

    if panel.len() > height {
        panel.truncate(height);
    }
    let top = height.saturating_sub(panel.len()) / 2;
    let cursor = cursor
        .filter(|(row, _)| *row < panel.len())
        .map(|(row, column)| (top + row, column.min(columns.saturating_sub(1))));
    let mut lines = Vec::with_capacity(height);
    lines.extend(std::iter::repeat_n(" ".repeat(columns), top));
    lines.extend(
        panel
            .into_iter()
            .map(|line| markdown::fit_width(&line, columns)),
    );
    lines.extend(std::iter::repeat_n(
        " ".repeat(columns),
        height.saturating_sub(lines.len()),
    ));
    (lines, cursor)
}

/// The scrolling item list, plus a detail area for the selected row on
/// compact screens.
#[expect(clippy::too_many_arguments)]
fn render_items(
    picker: &Picker,
    editor: &Editor,
    selection: &str,
    compact: bool,
    inner: usize,
    height: usize,
    boxed: &dyn Fn(&str) -> String,
    separator: &str,
    panel: &mut Vec<String>,
) {
    let detail_rows = if compact { COMPACT_DETAIL_ROWS } else { 0 };
    let capacity = height
        .saturating_sub(5 + detail_rows)
        .max(1)
        .min(picker.items.len().max(1));
    let mut start = picker.selected.saturating_sub(capacity / 2);
    start = start.min(picker.items.len().saturating_sub(capacity));
    let end = (start + capacity).min(picker.items.len());
    // A queued message being edited shows its draft in the selected row.
    let description_of = |absolute: usize, item: &PickerItem| {
        if absolute == picker.selected && picker.editing.is_some() {
            let value = editor.text();
            if value.is_empty() {
                "type a value below…".into()
            } else {
                value.replace('\n', " ")
            }
        } else {
            item.description.clone()
        }
    };
    for (index, item) in picker.items[start..end].iter().enumerate() {
        let absolute = start + index;
        let marker = if absolute == picker.selected {
            "›"
        } else {
            " "
        };
        let text = if compact {
            format!(" {marker} {}", item.label)
        } else {
            format!(
                " {marker} {}  ·  {}",
                item.label,
                description_of(absolute, item)
            )
        };
        if absolute == picker.selected {
            panel.push(boxed(&super::render::selected_row(
                &markdown::fit_width(&text, inner),
                selection,
            )));
        } else {
            panel.push(boxed(&text));
        }
    }
    if compact {
        let description = picker
            .items
            .get(picker.selected)
            .map(|item| description_of(picker.selected, item))
            .unwrap_or_default();
        panel.push(separator.to_string());
        let mut details =
            markdown::wrapped_plain_lines(&description, inner.saturating_sub(2).max(1));
        details.resize(COMPACT_DETAIL_ROWS - 1, String::new());
        for line in details {
            panel.push(boxed(&format!(" \x1b[2m{line}\x1b[0m")));
        }
    }
}
