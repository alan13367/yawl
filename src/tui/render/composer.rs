//! Composer chrome: borders with mode labels and key hints, the plan
//! handoff choices, and keeping a tall input within its rows.

use crate::config::UiColor;

use super::super::ViewState;
use super::super::input::Editor;
use super::super::markdown;
use super::super::picker::{Picker, PickerAction};
use super::{selected_row, selection_style};

/// What the keys do while a turn runs, for the composer's bottom border.
/// Enter steers a typed draft, Tab queues it, and Esc stops the turn even
/// with a draft, which is easy to press by mistake. Commands are never
/// steering, so their drafts only get the Esc hint. Falls back to shorter
/// hints when the border is narrow.
pub(super) fn busy_composer_hint(
    state: &ViewState,
    editor: &Editor,
    menu_hidden: bool,
    width: usize,
) -> Option<&'static str> {
    if state.turn_started.is_none()
        || state.question.is_some()
        || state.picker.is_some()
        || !menu_hidden
    {
        return None;
    }
    let candidates: &[&'static str] =
        if editor.is_empty() || editor.text().trim_start().starts_with('/') {
            &[" Esc stops "]
        } else {
            &[
                " Enter steers · Tab queues · Esc stops ",
                " Enter steers · Tab queues ",
                " Tab queues ",
            ]
        };
    candidates
        .iter()
        .copied()
        .find(|hint| markdown::visible_width(hint) < width)
}

pub(in crate::tui) fn composer_top_border(width: usize, label: Option<&str>) -> String {
    let Some(label) = label else {
        return "─".repeat(width);
    };
    let label = format!(" {label} ");
    if label.len().saturating_add(1) > width {
        return "─".repeat(width);
    }
    format!(
        "─{label}{}",
        "─".repeat(width.saturating_sub(label.len() + 1))
    )
}

pub(super) fn render_plan_handoff_input(
    picker: &Picker,
    width: usize,
    selection_color: UiColor,
    compact: bool,
) -> (Vec<String>, usize) {
    let mut lines = Vec::with_capacity(picker.items.len() + 2);
    let mut focus_row = 0;
    if !compact {
        lines.push(markdown::fit_width("Choose what happens next", width));
    }
    for (index, item) in picker.items.iter().enumerate() {
        if index == picker.selected {
            focus_row = lines.len();
        }
        lines.extend(render_choice_lines(
            index,
            &item.label,
            &item.description,
            matches!(item.action, PickerAction::ImplementPlan),
            index == picker.selected,
            width,
            selection_color,
        ));
    }
    if !compact {
        lines.push(markdown::fit_width(
            " ↑/↓ choose · 1–2 select · Enter confirm · Esc return",
            width,
        ));
    }
    (lines, focus_row)
}

pub(super) fn bounded_input_view(
    lines: Vec<String>,
    cursor: Option<(usize, usize)>,
    focus_row: usize,
    max_lines: usize,
) -> (Vec<String>, Option<(usize, usize)>) {
    let max_lines = max_lines.max(1);
    if lines.len() <= max_lines {
        return (lines, cursor);
    }
    let focus_row = cursor
        .map_or(focus_row, |(row, _)| row)
        .min(lines.len().saturating_sub(1));
    let start = focus_row
        .saturating_sub(max_lines / 2)
        .min(lines.len() - max_lines);
    let end = start + max_lines;
    let cursor = cursor
        .and_then(|(row, column)| (start..end).contains(&row).then_some((row - start, column)));
    (lines[start..end].to_vec(), cursor)
}

pub(super) fn render_choice_lines(
    index: usize,
    label: &str,
    description: &str,
    recommended: bool,
    selected: bool,
    width: usize,
    selection_color: UiColor,
) -> Vec<String> {
    let prefix = format!(" {}. ", index + 1);
    let continuation = " ".repeat(markdown::visible_width(&prefix));
    let recommended = if recommended { " (Recommended)" } else { "" };
    let content = if description.is_empty() {
        format!("{label}{recommended}")
    } else {
        format!("{label}{recommended} · {description}")
    };
    let style = selection_style(selection_color);
    markdown::wrapped_plain_prefixed_lines(&prefix, &continuation, &content, width)
        .into_iter()
        .map(|line| {
            let line = markdown::fit_width(&line, width);
            if selected {
                selected_row(&line, &style)
            } else {
                line
            }
        })
        .collect()
}
