//! Git dashboard panels, layout, and text presentation.

use super::diff::render_diff_pane;
use super::{
    COMMIT_BOX_HEIGHT, COMMIT_BOX_INPUT, COMMIT_BOX_TOP, CONFIRM_HEIGHT, CONFIRM_TOP, Confirm,
    ConfirmHit, FIXED_ROWS, FileAction, FileActionHit, FileAreaRow, GitFile, GitFocus, GitLayout,
    GitSection, GitView, HISTORY_CHROME_ROWS, MENU_HEIGHT, MENU_ITEMS, MENU_TOP,
    MIN_HISTORY_ENTRIES, file_area_rows,
};
use super::{sanitize_plain, truncate_visible};
use crate::config::UiColor;
use crate::tui::input::Editor;
use crate::tui::render::{
    HIDDEN_CURSOR, ImageSupport, foreground_color, selected_row, selection_style, status_style,
};
use crate::tui::{ViewState, markdown};

/// Short question for the confirm dialog (the hint bar appends the keys).
fn confirm_message(confirm: &Confirm) -> String {
    match confirm {
        Confirm::DiscardFile {
            path,
            renamed_from,
            untracked,
            staged,
        } => {
            let name = match renamed_from {
                Some(old) => format!("{old} -> {path}"),
                None => path.clone(),
            };
            let name = sanitize_plain(&name);
            if *untracked {
                format!("Delete untracked {name}?")
            } else if *staged {
                format!("Discard staged and unstaged changes in {name}?")
            } else {
                format!("Discard changes in {name}?")
            }
        }
        Confirm::DiscardAll => "Discard all unstaged changes?".to_string(),
    }
}

/// Dropdown rows: (label, description). The amend row reflects the flag.
fn commit_menu_items(amend: bool) -> [(&'static str, &'static str); 4] {
    [
        ("Commit", "Commit staged changes"),
        ("Commit and Push", "Commit, then push"),
        ("Commit and Sync", "Pull, commit, then push"),
        (
            if amend { "Amend: on" } else { "Amend: off" },
            "Toggle --amend",
        ),
    ]
}

/// Visible width of the trailing per-file action cell (`  ↩  +`): undo on
/// the left, stage/unstage on the right, daylight between them so a click
/// aimed at one cannot land on the other.
const ACTION_CELL_WIDTH: usize = 6;

/// Semantic colors for staging actions. These stay independent of the
/// configurable interface accent, which is also the default row-selection
/// background.
const STAGE_ACTION_FG: &str = "\x1b[38;2;139;213;162m";

const UNSTAGE_ACTION_FG: &str = "\x1b[38;2;232;202;118m";

/// Stage (`+`) for unstaged/untracked files, unstage (`-`) for staged ones.
fn row_action(file: &GitFile) -> char {
    match file.section {
        GitSection::Staged => '-',
        GitSection::Unstaged | GitSection::Untracked => '+',
    }
}

/// Trailing per-file action cell: dim undo hook left, semantic stage glyph
/// right. A selected glyph inherits the row's contrast-aware foreground so
/// it cannot disappear against the selection background.
fn action_cell_text(glyph: char, selected: bool) -> String {
    let color = if selected {
        ""
    } else if glyph == '+' {
        STAGE_ACTION_FG
    } else {
        UNSTAGE_ACTION_FG
    };
    format!("  \x1b[2m↩\x1b[0m  {color}\x1b[1m{glyph}\x1b[0m")
}

/// First visible row, shared by rendering and mouse hit-testing.
pub(super) fn file_area_start(view: &GitView, rows: &[FileAreaRow], file_capacity: usize) -> usize {
    view.file_scroll
        .min(rows.len().saturating_sub(file_capacity))
}

/// First visible history row for the current history scroll position.
pub(super) fn history_top(view: &GitView, visible: usize) -> usize {
    if view.history.len() <= visible {
        0
    } else {
        view.history_scroll.min(view.history.len() - visible)
    }
}

/// Splits the right panel between the file list and the history section so
/// history reaches the bottom instead of stopping after five rows.
///
/// Returns `(file_capacity, history_capacity, pad_between)` where `pad_between`
/// are blank rows inserted between the files and the history header to anchor
/// history to the bottom when everything fits. When overflowing, files keep
/// the top and history takes the remainder (at least [`MIN_HISTORY_ENTRIES`]
/// when the terminal allows), both scrollable.
fn panel_split(view: &GitView, content_height: usize) -> (usize, usize, usize) {
    let available = content_height.saturating_sub(FIXED_ROWS);
    let file_needed = if view.status.is_clean() {
        2
    } else {
        file_area_rows(view).len()
    };
    let history_total = view.history.len();
    if file_needed + history_total + HISTORY_CHROME_ROWS <= available {
        let pad = available - (file_needed + history_total + HISTORY_CHROME_ROWS);
        (file_needed.max(1), history_total, pad)
    } else {
        let max_file = available
            .saturating_sub(MIN_HISTORY_ENTRIES + HISTORY_CHROME_ROWS)
            .max(1);
        let mut file_capacity = file_needed.min(max_file).max(1);
        let remainder = available.saturating_sub(file_capacity + HISTORY_CHROME_ROWS);
        if remainder >= history_total {
            // Short history: show it all and hand the spare rows back to files.
            file_capacity = file_needed
                .min(
                    available
                        .saturating_sub(history_total + HISTORY_CHROME_ROWS)
                        .max(1),
                )
                .max(1);
            (file_capacity, history_total, 0)
        } else {
            (file_capacity, remainder.max(1), 0)
        }
    }
}

/// Muted selection-color background, distinct from the active file.
fn hovered_row(line: &str, color: UiColor) -> String {
    let tint = UiColor::new(
        24 + color.red / 8,
        24 + color.green / 8,
        24 + color.blue / 8,
    );
    selected_row(line, &selection_style(tint))
}

fn panel_width(columns: usize) -> usize {
    if columns < 60 {
        30.min(columns.saturating_sub(12)).max(20)
    } else {
        ((columns * 35) / 100).clamp(30, 44)
    }
}

const COMMIT_MENU_LABEL: &str = "│ ∨ (v)";

fn commit_button_width(width: usize) -> usize {
    width.saturating_sub(markdown::visible_width(COMMIT_MENU_LABEL))
}

pub(in crate::tui) fn render(
    state: &mut ViewState,
    editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(40);
    let rows = rows.max(10);
    let right_width = panel_width(columns).min(columns - 21);
    let divider = columns - right_width - 1;
    let left_width = divider;
    let content_height = rows - 1;

    // Files stay on top; history anchors to the bottom and takes the
    // remainder so it is no longer capped at five rows. `pad_between` pushes
    // history down when everything fits.
    let (file_capacity, history_capacity, pad_between) = state
        .git_view
        .as_ref()
        .map(|view| panel_split(view, content_height))
        .unwrap_or((1, 2, 0));
    {
        let Some(view) = state.git_view.as_mut() else {
            return (vec![" ".repeat(columns); rows], HIDDEN_CURSOR);
        };
        view.ensure_selection_visible(file_capacity);
        view.ensure_history_visible(history_capacity);
    }

    // Record hit-test geometry for mouse clicks.
    {
        let Some(view) = state.git_view.as_mut() else {
            return (vec![" ".repeat(columns); rows], HIDDEN_CURSOR);
        };
        let mut layout = GitLayout {
            divider,
            height: content_height,
            right_width,
            commit_row: COMMIT_BOX_INPUT,
            commit_box_top: COMMIT_BOX_TOP,
            commit_x0: divider + 1,
            commit_x1: divider + 1 + right_width,
            commit_button_row: COMMIT_BOX_TOP + COMMIT_BOX_HEIGHT,
            commit_button_x0: divider + 1,
            commit_button_x1: divider + 1 + commit_button_width(right_width),
            menu_button_x0: divider + 1 + commit_button_width(right_width),
            menu_button_x1: divider + 1 + right_width,
            close_button: None,
            file_rows: Vec::new(),
            actions: Vec::new(),
            stage_all_row: None,
            history_rows: Vec::new(),
            menu_rows: Vec::new(),
            confirm_rows: Vec::new(),
        };
        if view.commit_menu.is_some() {
            for item in 0..MENU_ITEMS {
                layout.menu_rows.push((MENU_TOP + 1 + item, item));
            }
        }
        // Clickable answers on the confirm box's answer row.
        if view.confirm.is_some() {
            let answers_row = CONFIRM_TOP + 2;
            let base = layout.divider + 1;
            layout.confirm_rows.push(ConfirmHit {
                row: answers_row,
                x0: base + 2,
                x1: base + 11,
                confirm: true,
            });
            layout.confirm_rows.push(ConfirmHit {
                row: answers_row,
                x0: base + 13,
                x1: base + 19,
                confirm: false,
            });
        }
        // File rows start after FIXED_ROWS. The shared helpers keep this
        // mapping identical to `render_right` so clicks land on the drawn
        // rows (headers consume rows too).
        let area = file_area_rows(view);
        let scroll = file_area_start(view, &area, file_capacity);
        let mut row = FIXED_ROWS;
        let action_x0 = layout.divider + 1 + layout.right_width.saturating_sub(6);
        let action_x1 = layout.divider + 1 + layout.right_width;
        // The cell reads `  ↩  +`: undo owns the left half, stage the right.
        let action_mid = action_x0 + 3;
        if view.status.is_clean() {
            // Mirrors the two "working tree clean" lines in `render_right`.
            row += 2;
        }
        for slot in area.iter().skip(scroll).take(file_capacity) {
            if matches!(slot, FileAreaRow::Header(GitSection::Unstaged)) {
                layout.stage_all_row = Some(row);
            }
            if let FileAreaRow::File(idx) = slot {
                layout.file_rows.push((row, *idx));
                if let Some(file) = view.flat.get(*idx) {
                    layout.actions.push(FileActionHit {
                        row,
                        x0: action_x0,
                        x1: action_mid,
                        index: *idx,
                        action: FileAction::Discard,
                    });
                    layout.actions.push(FileActionHit {
                        row,
                        x0: action_mid,
                        x1: action_x1,
                        index: *idx,
                        action: FileAction::Stage(!matches!(file.section, GitSection::Staged)),
                    });
                }
            }
            row += 1;
            if row >= content_height {
                break;
            }
        }
        // HISTORY rows follow the file area (plus bottom-anchoring padding):
        // header, entries, detail footer.
        let hist_top = history_top(view, history_capacity);
        row += pad_between;
        row += 1; // header row
        for (offset, _) in view
            .history
            .iter()
            .enumerate()
            .skip(hist_top)
            .take(history_capacity)
        {
            if row >= content_height {
                break;
            }
            layout.history_rows.push((row, offset));
            row += 1;
        }
        if view.diff.is_some() || view.show_log || view.show_branches {
            layout.close_button = Some((0, 0));
        }
        view.layout = layout;
    }

    let right_lines = render_right(
        state,
        right_width,
        content_height,
        file_capacity,
        history_capacity,
        pad_between,
    );
    let left_lines = render_left(state, editor, left_width, content_height);

    let mut frame = Vec::with_capacity(rows);
    for index in 0..content_height {
        let left = left_lines.get(index).cloned().unwrap_or_default();
        let right = right_lines.get(index).cloned().unwrap_or_default();
        let left = markdown::fit_width(&left, left_width);
        let right = markdown::fit_width(&right, right_width);
        frame.push(format!("{left}\x1b[2m│\x1b[0m{right}"));
    }
    let hint = hint_line(state);
    frame.push(markdown::fit_width(
        &format!(
            "{}{}\x1b[0m",
            status_style(
                state
                    .git_view
                    .as_ref()
                    .map_or(UiColor::WHITE, |_| accent_of(state))
            ),
            hint
        ),
        columns,
    ));

    let cursor = {
        let Some(view) = state.git_view.as_ref() else {
            return (frame, HIDDEN_CURSOR);
        };
        if view.focus == GitFocus::Commit && view.confirm.is_none() {
            let width = view.layout.right_width.saturating_sub(4).max(1);
            commit_cursor_position(
                view.layout.divider,
                view.layout.commit_row,
                &view.commit,
                view.commit_cursor,
                width,
                columns,
            )
        } else {
            HIDDEN_CURSOR
        }
    };
    (frame, cursor)
}

pub(super) fn accent_of(state: &ViewState) -> UiColor {
    state.accent_color
}

fn hint_line(state: &ViewState) -> String {
    if let Some(job) = &state.git_job
        && !job.label.is_empty()
    {
        return format!("{}  Ctrl+C cancels", job.label);
    }
    let Some(view) = state.git_view.as_ref() else {
        return String::new();
    };
    if let Some(confirm) = &view.confirm {
        return format!(
            " {} Enter confirms · Esc keeps it",
            confirm_message(confirm)
        );
    }
    if view.show_branches {
        return " ↑↓ select · Enter switch · n new from message box · b/Esc close".to_string();
    }
    match view.focus {
        GitFocus::Commit => {
            " Type message · Enter commit · Ctrl+G commit · Esc files · Tab panels".to_string()
        }
        GitFocus::Diff => {
            " ↑↓/PgUp/PgDn scroll · Space stage/unstage · Enter/Esc close diff".to_string()
        }
        GitFocus::History => " ↑↓ select commit · Enter view · Tab message · Esc files".to_string(),
        GitFocus::Files => {
            if view.show_log {
                " ↑↓ scroll log · l/Esc close · r refresh".to_string()
            } else if view.diff.is_some() {
                " ↑↓ files · Enter preview · Space/+/- stage · d discard · Tab panels · Esc close diff"
                    .to_string()
            } else {
                " ↑↓ move · Enter diff · Space/+/- stage · a all · d discard · Tab panels · e message · C commit · v menu · p push · b branches · l log · s stash · Esc close".to_string()
            }
        }
    }
}

pub(super) fn commit_visible_text(commit: &str, cursor: usize, width: usize) -> String {
    if commit.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = commit.chars().collect();
    if chars.len() <= width.max(1) {
        return commit.to_string();
    }
    // Keep the cursor visible: show the tail ending at the cursor.
    let end = cursor.min(chars.len());
    let start = end.saturating_sub(width.max(1));
    chars[start..end].iter().collect()
}

fn commit_visible(view: &GitView, width: usize) -> String {
    commit_visible_text(&view.commit, view.commit_cursor, width)
}

/// 1-based terminal coordinates for the commit-box cursor. The frame is drawn
/// with 1-based `CUP` positioning (`cursor_control`), so the row is the frame
/// index plus one and the column accounts for the divider, the box border,
/// the `"> "` prefix, and the visible cursor offset within the (possibly
/// scrolled) message.
fn commit_cursor_position(
    divider: usize,
    commit_row: usize,
    commit: &str,
    cursor: usize,
    inner_width: usize,
    columns: usize,
) -> (usize, usize) {
    let width = inner_width.max(1);
    let len = commit.chars().count();
    let cursor_in_shown = if len <= width {
        cursor.min(len)
    } else {
        commit_visible_text(commit, cursor, width).chars().count()
    };
    (
        commit_row + 1,
        (divider + 5 + cursor_in_shown).min(columns.saturating_sub(1)),
    )
}

/// HISTORY section below the changed files: its own scroll window over recent
/// commits plus a two-line detail footer for the hovered (or selected)
/// commit — full subject, hash, author, and date.
fn render_history_section(
    view: &GitView,
    accent: UiColor,
    selection_color: UiColor,
    lines: &mut Vec<String>,
    width: usize,
    visible: usize,
) {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    let count_label = if view.history_has_more {
        format!("HISTORY ({}+)", view.history.len())
    } else {
        format!("HISTORY ({})", view.history.len())
    };
    lines.push(markdown::fit_width(
        &format!("\x1b[1m{count_label}\x1b[0m"),
        width,
    ));
    if view.history.is_empty() {
        lines.push(markdown::fit_width(
            &format!("{DIM}No commits yet.{RESET}"),
            width,
        ));
        return;
    }
    let dot = foreground_color(accent);
    let selection = selection_style(selection_color);
    let top = history_top(view, visible);
    let hovered_history = view.hovered_history();
    let open_commit = view.diff.as_ref().and_then(|diff| diff.commit.as_deref());
    for (offset, entry) in view.history.iter().enumerate().skip(top).take(visible) {
        // Opening a commit moves focus to the diff pane for scrolling, but
        // the row that owns the open diff stays highlighted (matched by hash
        // so a concurrent reload cannot misattribute it).
        let is_open = view.focus == GitFocus::Diff && open_commit == Some(entry.hash.as_str());
        let is_selected =
            (offset == view.history_selected && view.focus == GitFocus::History) || is_open;
        let hovered = hovered_history == Some(offset) && !is_selected;
        let marker = if is_selected { "›" } else { " " };
        let subject = truncate_visible(
            &sanitize_plain(&entry.subject),
            width.saturating_sub(12).max(8),
        );
        let content = format!(
            "{marker} {dot}●{RESET} {} {subject}",
            sanitize_plain(&entry.short)
        );
        let fitted = markdown::fit_width(&content, width);
        if is_selected {
            lines.push(selected_row(&fitted, &selection));
        } else if hovered {
            lines.push(hovered_row(&fitted, selection_color));
        } else {
            lines.push(fitted);
        }
    }
    // Detail footer: hovered commit wins, otherwise the selected one.
    let detail = hovered_history
        .and_then(|index| view.history.get(index))
        .or_else(|| view.history.get(view.history_selected));
    match detail {
        Some(entry) => {
            lines.push(markdown::fit_width(
                &format!("◉ {}", sanitize_plain(&entry.subject)),
                width,
            ));
            let mut meta = sanitize_plain(&format!(
                "{} · {} · {}",
                entry.short, entry.author, entry.date
            ));
            if !entry.refs.is_empty() {
                meta.push_str(&format!(" · {}", sanitize_plain(&entry.refs)));
            }
            lines.push(markdown::fit_width(&format!("{DIM}{meta}{RESET}"), width));
        }
        None => {
            lines.push(markdown::fit_width(
                &format!("{DIM}No history yet.{RESET}"),
                width,
            ));
            lines.push(markdown::fit_width("", width));
        }
    }
}

fn render_right(
    state: &mut ViewState,
    width: usize,
    height: usize,
    file_capacity: usize,
    history_rows: usize,
    pad_between: usize,
) -> Vec<String> {
    let Some(view) = state.git_view.as_ref() else {
        return vec![String::new(); height];
    };
    let accent = foreground_color(state.accent_color);
    let mut lines = Vec::with_capacity(height);
    // Title row.
    let ahead_behind = match (view.status.ahead, view.status.behind) {
        (0, 0) => String::new(),
        (a, 0) => format!(" ↑{a}"),
        (0, b) => format!(" ↓{b}"),
        (a, b) => format!(" ↑{a}↓{b}"),
    };
    lines.push(markdown::fit_width(
        &format!(
            "{accent}\x1b[1mCHANGES\x1b[0m \x1b[2m▾ {}{ahead_behind}",
            sanitize_plain(&view.status.branch)
        ),
        width,
    ));
    // Commit message box: a labeled three-row box so the input reads as an
    // input even before it is focused. The border takes the accent color
    // while editing and stays dim otherwise.
    let editing = view.focus == GitFocus::Commit;
    let frame = if editing {
        foreground_color(state.accent_color)
    } else {
        "\x1b[2m".to_string()
    };
    let amend_tag = if view.amend && width >= 22 {
        " (amend)"
    } else {
        ""
    };
    let title = format!("Message{amend_tag}");
    let dashes = "─".repeat(width.saturating_sub(title.len() + 5));
    lines.push(markdown::fit_width(
        &format!("{frame}╭─ \x1b[1m{title}\x1b[0m{frame} {dashes}╮\x1b[0m"),
        width,
    ));
    let text_width = width.saturating_sub(4).max(1);
    let commit_inner = if view.commit.is_empty() {
        if editing {
            "\x1b[2mType a message · Enter to commit…\x1b[0m".to_string()
        } else {
            "\x1b[2mMessage (e to edit)…\x1b[0m".to_string()
        }
    } else {
        sanitize_plain(&commit_visible(view, text_width))
    };
    let prompt = if editing {
        format!("{}> \x1b[0m", foreground_color(state.accent_color))
    } else {
        "> ".to_string()
    };
    let input_body = markdown::fit_width(
        &format!("{prompt}{}", markdown::fit_width(&commit_inner, text_width)),
        width.saturating_sub(2),
    );
    lines.push(markdown::fit_width(
        &format!("{frame}│\x1b[0m{input_body}{frame}│\x1b[0m"),
        width,
    ));
    lines.push(markdown::fit_width(
        &format!(
            "{frame}╰{}╯\x1b[0m",
            "─".repeat(width.saturating_sub(2).max(2))
        ),
        width,
    ));
    // Commit button row.
    let amend = if view.amend { " (amend)" } else { "" };
    let button = format!(
        "{}\x1b[2m{COMMIT_MENU_LABEL}\x1b[0m",
        markdown::fit_width(
            &format!("\x1b[1m✓ Commit{amend}\x1b[0m"),
            commit_button_width(width)
        )
    );
    lines.push(markdown::fit_width(&button, width));
    // Meta row.
    let meta = format!(
        "\x1b[2m{} · {} staged · {} unstaged\x1b[0m",
        sanitize_plain(view.status.upstream.as_deref().unwrap_or("no upstream")),
        view.status.staged.len(),
        view.status.unstaged.len() + view.status.untracked.len()
    );
    lines.push(markdown::fit_width(&meta, width));
    // Notice row.
    if view.notice.is_empty() {
        lines.push(markdown::fit_width("", width));
    } else if view.notice_error {
        lines.push(markdown::fit_width(
            &format!("\x1b[1;31m{}\x1b[0m", sanitize_plain(&view.notice)),
            width,
        ));
    } else {
        lines.push(markdown::fit_width(
            &format!("\x1b[2m{}\x1b[0m", sanitize_plain(&view.notice)),
            width,
        ));
    }
    lines.push(markdown::fit_width("", width));

    if view.status.is_clean() {
        lines.push(markdown::fit_width(
            "\x1b[2mWorking tree clean.\x1b[0m",
            width,
        ));
        lines.push(markdown::fit_width(
            "\x1b[2mStage files with git add outside, then press r.\x1b[0m",
            width,
        ));
    } else {
        // Window the shared file-area rows by the flat scroll position.
        let rows = file_area_rows(view);
        let start = file_area_start(view, &rows, file_capacity);
        let selection_style = selection_style(state.selection_color);
        let hovered_file = view.hovered_file();
        for row in rows.iter().skip(start).take(file_capacity) {
            match row {
                FileAreaRow::Header(section) => {
                    let count = match section {
                        GitSection::Staged => view.status.staged.len(),
                        GitSection::Unstaged => {
                            view.status.unstaged.len() + view.status.untracked.len()
                        }
                        GitSection::Untracked => view.status.untracked.len(),
                    };
                    let title = format!("\x1b[1m{} ({count})\x1b[0m", section.title());
                    if *section == GitSection::Unstaged {
                        lines.push(format!(
                            "{}\x1b[1m  +\x1b[0m",
                            markdown::fit_width(&title, width.saturating_sub(3))
                        ));
                    } else {
                        lines.push(markdown::fit_width(&title, width));
                    }
                }
                FileAreaRow::File(i) => {
                    let file = &view.flat[*i];
                    // A commit diff belongs to history, not to any worktree
                    // file: clear the file highlight so the panel does not
                    // look like a file diff is still open.
                    let commit_open = view.diff.as_ref().is_some_and(|diff| diff.commit.is_some());
                    let selected =
                        *i == view.selected && view.focus != GitFocus::Commit && !commit_open;
                    let hovered = hovered_file == Some(*i) && !selected;
                    let marker = if selected { "›" } else { " " };
                    let label = file.status_label();
                    let name = if let Some(from) = &file.renamed_from {
                        format!("{from} → {}", file.path)
                    } else {
                        file.path.clone()
                    };
                    // Trailing stage/unstage (`+`/`-`) and undo (`↩`)
                    // affordances, clickable without opening the diff.
                    let glyph = row_action(file);
                    let prefix = format!("{marker} {label} ");
                    let name_width = width
                        .saturating_sub(markdown::visible_width(&prefix))
                        .saturating_sub(ACTION_CELL_WIDTH)
                        .max(1);
                    let name = truncate_visible(&sanitize_plain(&name), name_width);
                    let name_pad =
                        " ".repeat(name_width.saturating_sub(markdown::visible_width(&name)));
                    let action_text = action_cell_text(glyph, selected);
                    let content = format!("{prefix}{name}{name_pad}{action_text}");
                    let fitted = markdown::fit_width(&content, width);
                    if selected {
                        lines.push(selected_row(&fitted, &selection_style));
                    } else if hovered {
                        lines.push(hovered_row(&fitted, state.selection_color));
                    } else {
                        lines.push(fitted);
                    }
                }
            }
        }
    }
    // Bottom-anchoring gap: when files and history both fit, empty rows sit
    // between them so history ends at the panel bottom instead of floating
    // right under the file list.
    for _ in 0..pad_between {
        lines.push(markdown::fit_width("", width));
    }
    render_history_section(
        view,
        state.accent_color,
        state.selection_color,
        &mut lines,
        width,
        history_rows,
    );
    // Commit-options dropdown occludes whatever sits under it (meta, notice,
    // top file rows) without disturbing their scroll state.
    if view.commit_menu.is_some() {
        let menu = render_commit_menu(
            view.commit_menu
                .as_ref()
                .map(|menu| menu.selected)
                .unwrap_or(0),
            view.amend,
            &selection_style(state.selection_color),
            width,
        );
        for (offset, line) in menu.into_iter().enumerate() {
            let row = MENU_TOP + offset;
            if row < lines.len() {
                lines[row] = line;
            } else {
                while lines.len() < row {
                    lines.push(" ".repeat(width));
                }
                lines.push(line);
            }
        }
    }
    // Modal confirm box occludes the file area. Rendered after the commit
    // menu so it always wins; both preserve the scroll state underneath.
    if let Some(confirm) = view.confirm.as_ref() {
        for (offset, line) in render_confirm_box(confirm, width).into_iter().enumerate() {
            let row = CONFIRM_TOP + offset;
            if row < lines.len() {
                lines[row] = line;
            } else {
                while lines.len() < row {
                    lines.push(" ".repeat(width));
                }
                lines.push(line);
            }
        }
    }
    while lines.len() < height {
        lines.push(" ".repeat(width));
    }
    lines.truncate(height);
    // Fit every line to the panel width (keeps ANSI widths exact).
    lines
        .into_iter()
        .map(|line| markdown::fit_width(&line, width))
        .collect()
}

/// Modal confirm box anchored over the file area. Exactly `CONFIRM_HEIGHT`
/// rows; the answers on row 2 are clickable (see `GitLayout::confirm_rows`).
fn render_confirm_box(confirm: &Confirm, width: usize) -> Vec<String> {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    const RED_BOLD: &str = "\x1b[1;31m";
    let inner = width.saturating_sub(2).max(6);
    let mut top = format!("{DIM}╭─ Confirm ");
    let used = markdown::visible_width(&top);
    top.push_str(&"─".repeat(inner.saturating_sub(used)));
    top.push_str(&format!("╮{RESET}"));
    let message = truncate_visible(
        &sanitize_plain(&confirm_message(confirm)),
        inner.saturating_sub(2).max(8),
    );
    let answers = format!("  {RED_BOLD}[Discard]{RESET}  {DIM}[Keep]{RESET}");
    let mut bottom = format!("{DIM}╰─ Enter confirm · Esc keep ");
    let bottom_used = markdown::visible_width(&bottom);
    bottom.push_str(&"─".repeat(inner.saturating_sub(bottom_used)));
    bottom.push_str(&format!("╯{RESET}"));
    let mut rows = vec![
        markdown::fit_width(&top, width),
        markdown::fit_width(&format!("│ {message}"), width),
        markdown::fit_width(&answers, width),
        markdown::fit_width(&bottom, width),
    ];
    debug_assert_eq!(rows.len(), CONFIRM_HEIGHT);
    rows.truncate(CONFIRM_HEIGHT);
    while rows.len() < CONFIRM_HEIGHT {
        rows.push(" ".repeat(width));
    }
    rows
}

/// Dropdown box anchored under the commit button. Exactly `MENU_HEIGHT` rows.
fn render_commit_menu(selected: usize, amend: bool, selection: &str, width: usize) -> Vec<String> {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    let inner = width.saturating_sub(2).max(4);
    let rule = |left: char, fill: &str, right: char| {
        let mut line = format!("{left}{fill} ");
        line.push_str(&truncate_visible("Commit options", inner.saturating_sub(2)));
        let used = markdown::visible_width(&line);
        line.push_str(&fill.repeat(inner.saturating_sub(used)));
        line.push(right);
        markdown::fit_width(&format!("{DIM}{line}{RESET}"), width)
    };
    let mut lines = vec![rule('╭', "─", '╮')];
    for (index, (label, _)) in commit_menu_items(amend).iter().enumerate() {
        let marker = if index == selected { "›" } else { " " };
        let content = format!("│{marker} {label}");
        let fitted = markdown::fit_width(&content, width.saturating_sub(1)) + "│";
        let fitted = markdown::fit_width(&fitted, width);
        if index == selected {
            lines.push(selected_row(&fitted, selection));
        } else {
            lines.push(fitted);
        }
    }
    lines.push(markdown::fit_width(
        &format!("{DIM}╰─ Enter run · Esc close ─╯{RESET}"),
        width,
    ));
    debug_assert_eq!(lines.len(), MENU_HEIGHT);
    lines.truncate(MENU_HEIGHT);
    while lines.len() < MENU_HEIGHT {
        lines.push(" ".repeat(width));
    }
    lines
}

fn render_left(state: &mut ViewState, editor: &Editor, width: usize, height: usize) -> Vec<String> {
    let (show_branches, show_log, has_diff) = state
        .git_view
        .as_ref()
        .map(|view| (view.show_branches, view.show_log, view.diff.is_some()))
        .unwrap_or((false, false, false));
    if show_branches {
        return render_branches(state, width, height);
    }
    if show_log {
        return render_log(state, width, height);
    }
    if has_diff {
        return render_diff_pane(state, width, height);
    }
    // Chat transcript stays visible behind the panel.
    let window =
        crate::tui::render::render_transcript_window(state, width, height, ImageSupport::None);
    let mut lines: Vec<String> = window
        .lines
        .iter()
        .map(|(line, _)| markdown::fit_width(line, width))
        .collect();
    // Pad the top so short transcripts sit at the bottom like the main view.
    while lines.len() < height {
        lines.insert(0, " ".repeat(width));
    }
    lines.truncate(height);
    let _ = editor;
    lines
}

pub(super) fn render_branches(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
    let Some(view) = state.git_view.as_ref() else {
        return vec![String::new(); height];
    };
    let mut lines = vec![markdown::fit_width(
        "\x1b[1mBranches\x1b[0m \x1b[2mEnter switch · n new from message · Esc close\x1b[0m",
        width,
    )];
    if view.branches.is_empty() {
        lines.push(markdown::fit_width(
            "\x1b[2mNo branches found.\x1b[0m",
            width,
        ));
    } else {
        let selection = selection_style(state.selection_color);
        for (index, branch) in view.branches.iter().enumerate() {
            let current = branch == &view.status.branch;
            let marker = if index == view.branch_selected {
                "›"
            } else {
                " "
            };
            let current_mark = if current { "● " } else { "  " };
            let content = format!("{marker} {current_mark}{}", sanitize_plain(branch));
            let fitted = markdown::fit_width(&content, width);
            if index == view.branch_selected {
                lines.push(selected_row(&fitted, &selection));
            } else {
                lines.push(fitted);
            }
        }
    }
    while lines.len() < height {
        lines.push(" ".repeat(width));
    }
    lines.truncate(height);
    lines
}

pub(super) fn render_log(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
    let Some(view) = state.git_view.as_mut() else {
        return vec![String::new(); height];
    };
    let mut lines = vec![markdown::fit_width(
        "\x1b[1mLog\x1b[0m \x1b[2m↑↓ scroll · l/Esc close\x1b[0m",
        width,
    )];
    if view.history.is_empty() {
        lines.push(markdown::fit_width("\x1b[2mNo commits yet.\x1b[0m", width));
    } else {
        let body = height.saturating_sub(1);
        let max_top = view.history.len().saturating_sub(body);
        view.log_scroll = view.log_scroll.min(max_top);
        let top = view.log_scroll;
        for entry in view.history.iter().skip(top).take(body) {
            lines.push(markdown::fit_width(
                &format!(
                    "{} {} {}",
                    sanitize_plain(&entry.short),
                    sanitize_plain(&entry.subject),
                    if entry.refs.is_empty() {
                        String::new()
                    } else {
                        format!("({})", sanitize_plain(&entry.refs))
                    }
                ),
                width,
            ));
        }
    }
    while lines.len() < height {
        lines.push(" ".repeat(width));
    }
    lines.truncate(height);
    lines
}

#[cfg(test)]
mod tests {
    use super::super::repository::parse_status_porcelain;
    use super::super::{GitStatus, HistoryEntry};
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn commit_cursor_uses_one_based_terminal_coordinates() {
        // Divider at 64, input row is frame index 2: the cursor belongs on
        // the third screen line, after "│> " plus the typed text.
        assert_eq!(commit_cursor_position(64, 2, "rrw", 3, 30, 100), (3, 72));
        assert_eq!(commit_cursor_position(64, 2, "", 0, 30, 100), (3, 69));
        // Long messages scroll; the cursor stays on the visible tail.
        let long = "m".repeat(40);
        let (row, col) = commit_cursor_position(64, 2, &long, 40, 30, 100);
        assert_eq!(row, 3);
        assert!(col > 70 && col < 100);
    }

    #[test]
    fn file_rows_offer_stage_or_unstage_actions() {
        let staged = GitFile {
            path: "a.rs".into(),
            x: 'M',
            y: ' ',
            section: GitSection::Staged,
            renamed_from: None,
        };
        let unstaged = GitFile {
            path: "b.rs".into(),
            x: ' ',
            y: 'M',
            section: GitSection::Unstaged,
            renamed_from: None,
        };
        let untracked = GitFile {
            path: "c.rs".into(),
            x: '?',
            y: '?',
            section: GitSection::Untracked,
            renamed_from: None,
        };
        assert_eq!(row_action(&staged), '-');
        assert_eq!(row_action(&unstaged), '+');
        assert_eq!(row_action(&untracked), '+');
    }

    #[test]
    fn hovered_rows_keep_their_width_and_restore_background_after_resets() {
        let text = "  M src/main.rs  \x1b[33m-\x1b[0m  ";
        let row = hovered_row(text, UiColor::WHITE);
        assert_eq!(markdown::visible_width(&row), markdown::visible_width(text));
        assert_eq!(markdown::strip_ansi(&row), markdown::strip_ansi(text));
        assert!(row.contains("\x1b[0m\x1b[38;2;250;250;250;48;2;53;53;53m  "));
    }

    #[test]
    fn commit_menu_lists_push_sync_and_amend() {
        let labels: Vec<&str> = commit_menu_items(false)
            .iter()
            .map(|(label, _)| *label)
            .collect();
        assert_eq!(
            labels,
            vec!["Commit", "Commit and Push", "Commit and Sync", "Amend: off"]
        );
        assert_eq!(commit_menu_items(true)[3].0, "Amend: on");
    }

    #[test]
    fn action_cell_puts_undo_left_and_stage_right_with_daylight() {
        let cell = action_cell_text('+', false);
        assert_eq!(markdown::visible_width(&cell), ACTION_CELL_WIDTH);
        let plain = markdown::strip_ansi(&cell);
        let undo = plain.find('↩').expect("undo hook should be rendered");
        let stage = plain.find('+').expect("stage glyph should be rendered");
        assert!(undo < stage, "undo must sit left of stage");
        assert!(
            stage - undo >= 2,
            "glyphs need breathing room, got {plain:?}"
        );
    }

    #[test]
    fn selected_stage_action_inherits_contrasting_row_text() {
        // Use the stage green itself as the selection background. Reusing the
        // semantic foreground here would make `+` disappear.
        let selection = selection_style(UiColor::new(139, 213, 162));
        let cell = action_cell_text('+', true);
        let selected = selected_row(&cell, &selection);

        assert!(!cell.contains(STAGE_ACTION_FG));
        assert!(selected.contains(&selection));
        assert!(markdown::strip_ansi(&selected).contains('+'));
    }

    #[test]
    fn confirm_messages_name_the_destructive_action() {
        assert_eq!(
            confirm_message(&Confirm::DiscardFile {
                path: "a.rs".into(),
                renamed_from: None,
                untracked: true,
                staged: false,
            }),
            "Delete untracked a.rs?"
        );
        assert_eq!(
            confirm_message(&Confirm::DiscardFile {
                path: "a.rs".into(),
                renamed_from: None,
                untracked: false,
                staged: true,
            }),
            "Discard staged and unstaged changes in a.rs?"
        );
        assert_eq!(
            confirm_message(&Confirm::DiscardFile {
                path: "b.rs".into(),
                renamed_from: Some("a.rs".into()),
                untracked: false,
                staged: true,
            }),
            "Discard staged and unstaged changes in a.rs -> b.rs?"
        );
        assert_eq!(
            confirm_message(&Confirm::DiscardAll),
            "Discard all unstaged changes?"
        );
    }

    #[test]
    fn confirm_box_is_four_clickable_rows() {
        let confirm = Confirm::DiscardFile {
            path: "src/tui/git.rs".into(),
            renamed_from: None,
            untracked: false,
            staged: false,
        };
        let rows = render_confirm_box(&confirm, 44);
        assert_eq!(rows.len(), CONFIRM_HEIGHT);
        assert!(rows.iter().all(|row| markdown::visible_width(row) <= 44));
        let plain = rows
            .iter()
            .map(|row| markdown::strip_ansi(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("[Discard]"));
        assert!(plain.contains("[Keep]"));
        assert!(plain.contains("src/tui/git.rs"));
    }

    #[test]
    fn panel_width_stays_usable_on_small_terminals() {
        assert!(panel_width(40) >= 20);
        assert!(panel_width(200) <= 44);
    }

    #[test]
    fn history_panel_expands_beyond_five_rows_and_anchors_to_bottom() {
        // Clean tree with 30 commits on a 30-row terminal: files need 2 rows,
        // chrome needs 8 + 3, leaving 16 rows for history (not 5).
        let mut view = GitView::new(PathBuf::from("/unused"), GitStatus::default());
        view.history = (0..30)
            .map(|index| HistoryEntry {
                hash: format!("hash{index:03}"),
                short: format!("{index:07x}"),
                subject: format!("commit {index}"),
                ..HistoryEntry::default()
            })
            .collect();
        let (file_capacity, history_capacity, pad) = panel_split(&view, 29);
        assert_eq!(file_capacity, 2);
        assert_eq!(
            history_capacity, 16,
            "history must fill the panel, not stop at 5"
        );
        assert_eq!(pad, 0, "overflowing panels need no anchoring gap");

        // Few commits: show them all and pad between files and history so the
        // section ends at the panel bottom.
        view.history.truncate(3);
        let (file_capacity, history_capacity, pad) = panel_split(&view, 29);
        assert_eq!(history_capacity, 3);
        assert_eq!(file_capacity, 2);
        assert_eq!(
            pad,
            29 - FIXED_ROWS - (2 + 3 + 3),
            "empty rows sit above history to anchor it to the bottom"
        );

        // Many files: history keeps a usable minimum instead of collapsing.
        view.status = parse_status_porcelain(
            "## main\0M  a0\0M  a1\0M  a2\0M  a3\0M  a4\0M  a5\0M  a6\0M  a7\0M  a8\0M  a9\0M  a10\0M  a11\0M  a12\0M  a13\0M  a14\0M  a15\0M  a16\0M  a17\0M  a18\0M  a19\0",
        );
        view.flat = view.status.flat();
        view.history = (0..30)
            .map(|index| HistoryEntry {
                hash: format!("hash{index:03}"),
                ..HistoryEntry::default()
            })
            .collect();
        let (file_capacity, history_capacity, pad) = panel_split(&view, 29);
        assert_eq!(pad, 0);
        assert!(
            history_capacity >= MIN_HISTORY_ENTRIES,
            "history keeps at least {MIN_HISTORY_ENTRIES} rows, got {history_capacity}"
        );
        assert_eq!(
            file_capacity + history_capacity + HISTORY_CHROME_ROWS,
            29 - FIXED_ROWS,
            "overflowing panels fill the available height"
        );
    }
}
