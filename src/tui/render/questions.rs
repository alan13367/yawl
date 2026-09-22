//! Question composer layout, shared keyboard behavior, and readable option descriptions.

use super::{markdown, render_choice_lines};
use crate::config::UiColor;

pub(super) fn render_input(
    active: &crate::tui::state::ActiveQuestion,
    width: usize,
    selection_color: UiColor,
    compact: bool,
) -> (Vec<String>, Option<(usize, usize)>, usize) {
    let snapshot = &active.snapshot;
    let countdown = snapshot
        .remaining
        .map(|remaining| format!(" · {}s", remaining.as_secs().saturating_add(1)))
        .unwrap_or_default();
    let heading = if compact {
        format!(
            "{}/{}{countdown} · {}",
            snapshot.question_index + 1,
            snapshot.question_count,
            snapshot.question.question
        )
    } else {
        format!(
            "Question {}/{}{countdown}",
            snapshot.question_index + 1,
            snapshot.question_count
        )
    };
    let mut lines = markdown::wrapped_plain_lines(&heading, width);
    if !compact {
        lines.extend(
            markdown::wrapped_plain_lines(&snapshot.question.question, width)
                .into_iter()
                .map(|line| format!("\x1b[1m{line}\x1b[22m")),
        );
        lines.push(String::new());
    }

    if let crate::tui::state::QuestionInput::Custom(editor) = &active.input {
        if !compact {
            lines.push(String::new());
            lines.push(markdown::fit_width(" Your answer", width));
        }
        let layout = editor.layout(width);
        let answer_start = lines.len();
        let cursor = (answer_start + layout.cursor_row, layout.cursor_col);
        lines.extend(
            layout
                .lines
                .into_iter()
                .map(|line| markdown::fit_width(&line, width)),
        );
        if !compact {
            lines.push(markdown::fit_width(
                " Enter submit · Shift+Enter newline · Esc choices · Ctrl+C cancel",
                width,
            ));
            lines.push(String::new());
        }
        return (lines, Some(cursor), cursor.0);
    }

    let mut focus_row = lines.len();
    for (index, option) in snapshot.question.options.iter().enumerate() {
        if index == active.selected {
            focus_row = lines.len();
        }
        lines.extend(render_question_choice(
            index,
            &option.label,
            if compact { "" } else { &option.description },
            index == snapshot.question.recommended,
            index == active.selected,
            width,
            selection_color,
        ));
    }
    let other_index = snapshot.question.options.len();
    if other_index == active.selected {
        focus_row = lines.len();
    }
    lines.extend(render_question_choice(
        other_index,
        "Other…",
        if compact { "" } else { "Write your own answer" },
        false,
        other_index == active.selected,
        width,
        selection_color,
    ));
    if !compact {
        let option_count = snapshot.question.options.len() + 1;
        lines.push(markdown::fit_width(
            &format!(" ↑/↓ choose · 1–{option_count} select · Enter confirm · Esc cancel"),
            width,
        ));
        lines.push(String::new());
    }
    (lines, None, focus_row)
}

fn render_question_choice(
    index: usize,
    label: &str,
    description: &str,
    recommended: bool,
    selected: bool,
    width: usize,
    color: UiColor,
) -> Vec<String> {
    let mut lines = render_choice_lines(index, label, "", recommended, selected, width, color);
    if !description.is_empty() {
        lines.extend(
            markdown::wrapped_plain_prefixed_lines("    ", "    ", description, width)
                .into_iter()
                .map(|line| format!("\x1b[2m{}\x1b[22m", markdown::fit_width(&line, width))),
        );
    }
    lines
}
