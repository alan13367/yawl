//! Git dashboard keyboard, mouse, and commit editing.

use super::diff::resolve_diff_scroll;
use super::refresh;
use super::render::history_top;
use super::repository::{load_branches, load_history_limit, run_git};
use super::{
    CommitMenuState, CommitMode, Confirm, FileAction, GitFocus, GitSection, GitView,
    HISTORY_PRELOAD_THRESHOLD, INITIAL_HISTORY_LIMIT, MENU_ITEMS, MouseTarget, jobs, operations,
    poll_tick,
};
use crate::tui::ViewState;
use crate::tui::events::{Event, Key, MouseEvent, MouseKind};
use crate::tui::input::Editor;
use crate::tui::terminal::ScreenPoint;
use std::path::PathBuf;

/// Starts a background page-in when the selection, the history window, or the
/// full log sits within [`HISTORY_PRELOAD_THRESHOLD`] rows of the oldest
/// loaded commit. No-op while another git job runs or no more history is
/// expected, so rapid scrolling issues at most one extra load at a time.
fn maybe_load_more_history(state: &mut ViewState) {
    let needs_more = state.git_view.as_ref().is_some_and(|view| {
        if !view.history_has_more || view.history.is_empty() {
            return false;
        }
        let loaded = view.history.len();
        let near_selected = view.history_selected + HISTORY_PRELOAD_THRESHOLD >= loaded;
        let visible = view.layout.history_rows.len();
        let near_window = view.history_scroll + visible + HISTORY_PRELOAD_THRESHOLD >= loaded;
        let log_visible = view.layout.height.saturating_sub(1);
        let near_log =
            view.show_log && view.log_scroll + log_visible + HISTORY_PRELOAD_THRESHOLD >= loaded;
        near_selected || near_window || near_log
    });
    if !needs_more {
        return;
    }
    jobs::start(
        state,
        "Loading more history…",
        operations::load_more_history,
    );
}

/// Single-line text editing shared by the commit box and the init-modal URL
/// field: insertion, deletion, and word/line motions over char indices.
pub(super) fn line_insert(text: &mut String, cursor: &mut usize, insert: &str) {
    let mut chars: Vec<char> = text.chars().collect();
    let at = (*cursor).min(chars.len());
    let insert: Vec<char> = insert.chars().filter(|c| *c != '\n').collect();
    let count = insert.len();
    chars.splice(at..at, insert);
    *text = chars.into_iter().collect();
    *cursor = at + count;
}

pub(super) fn line_edit(text: &mut String, cursor: &mut usize, key: Key) -> bool {
    match key {
        Key::Char(c) => {
            line_insert(text, cursor, &c.to_string());
            true
        }
        Key::Backspace => {
            let mut chars: Vec<char> = text.chars().collect();
            if *cursor > 0 && *cursor <= chars.len() {
                chars.remove(*cursor - 1);
                *text = chars.into_iter().collect();
                *cursor -= 1;
            }
            true
        }
        Key::Delete => {
            let mut chars: Vec<char> = text.chars().collect();
            if *cursor < chars.len() {
                chars.remove(*cursor);
                *text = chars.into_iter().collect();
            }
            true
        }
        Key::Left => {
            *cursor = cursor.saturating_sub(1);
            true
        }
        Key::Right => {
            *cursor = (*cursor + 1).min(text.chars().count());
            true
        }
        Key::Home | Key::Ctrl('a') => {
            *cursor = 0;
            true
        }
        Key::End | Key::Ctrl('e') => {
            *cursor = text.chars().count();
            true
        }
        Key::Ctrl('u') => {
            let chars: Vec<char> = text.chars().collect();
            let at = (*cursor).min(chars.len());
            *text = chars[at..].iter().collect();
            *cursor = 0;
            true
        }
        Key::Ctrl('k') => {
            let chars: Vec<char> = text.chars().collect();
            let at = (*cursor).min(chars.len());
            *text = chars[..at].iter().collect();
            true
        }
        Key::Ctrl('w') => {
            let mut chars: Vec<char> = text.chars().collect();
            let mut at = (*cursor).min(chars.len());
            while at > 0 && chars[at - 1].is_whitespace() {
                chars.remove(at - 1);
                at -= 1;
            }
            while at > 0 && !chars[at - 1].is_whitespace() {
                chars.remove(at - 1);
                at -= 1;
            }
            *text = chars.into_iter().collect();
            *cursor = at;
            true
        }
        _ => false,
    }
}

fn commit_insert(view: &mut GitView, text: &str) {
    line_insert(&mut view.commit, &mut view.commit_cursor, text);
}

fn commit_key(view: &mut GitView, key: Key) -> bool {
    line_edit(&mut view.commit, &mut view.commit_cursor, key)
}

fn do_commit(state: &mut ViewState) {
    do_commit_with_mode(state, CommitMode::Commit);
}

fn do_commit_with_mode(state: &mut ViewState, mode: CommitMode) {
    jobs::start(state, "Committing…", move |state| {
        operations::do_commit_with_mode(state, mode)
    });
}

/// Runs the highlighted dropdown row. The amend row only flips the flag and
/// keeps the menu open; the rest commit and close it.
fn activate_commit_menu(state: &mut ViewState) {
    let selected = state
        .git_view
        .as_ref()
        .and_then(|view| view.commit_menu.as_ref())
        .map(|menu| menu.selected)
        .unwrap_or(0);
    match selected {
        0 => do_commit_with_mode(state, CommitMode::Commit),
        1 => do_commit_with_mode(state, CommitMode::AndPush),
        2 => do_commit_with_mode(state, CommitMode::AndSync),
        3 => {
            if let Some(view) = state.git_view.as_mut() {
                view.amend = !view.amend;
            }
        }
        _ => {
            if let Some(view) = state.git_view.as_mut() {
                view.commit_menu = None;
            }
        }
    }
}

pub(super) fn open_selected_diff(state: &mut ViewState) {
    jobs::start(state, "Loading diff…", operations::open_selected_diff);
}

/// Shows a historical commit's full diff in the left pane.
fn open_commit_diff(state: &mut ViewState) {
    jobs::start(state, "Loading commit…", operations::open_commit_diff);
}

pub(super) fn set_file_staged(state: &mut ViewState, index: usize, staged: bool) {
    jobs::start(state, "Updating index…", move |state| {
        operations::set_file_staged(state, index, staged)
    });
}

fn toggle_stage_selected(state: &mut ViewState) {
    let (index, stage) = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        match view.selected_file() {
            Some(file) => (view.selected, !matches!(file.section, GitSection::Staged)),
            None => return,
        }
    };
    set_file_staged(state, index, stage);
}

fn run_simple(state: &mut ViewState, args: &[&str], ok_message: &str) {
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let message = ok_message.to_string();
    jobs::start(state, "Running git…", move |state| {
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        operations::run_simple(state, &args, &message);
    });
}

pub(super) fn confirm_action(state: &mut ViewState) {
    if jobs::start(state, "Discarding…", operations::confirm_action)
        && let Some(view) = state.git_view.as_mut()
    {
        view.confirm = None;
    }
}

pub(in crate::tui) fn handle_event(state: &mut ViewState, _editor: &mut Editor, event: Event) {
    if matches!(event, Event::Key(Key::Escape)) {
        jobs::cancel(state);
    }

    if matches!(event, Event::Key(Key::Ctrl('c'))) && jobs::cancel(state) {
        return;
    }

    if matches!(event, Event::Tick) {
        poll_tick(state);
    }
    if state.git_view.is_none() {
        return;
    }
    match event {
        Event::Tick => {}
        Event::FocusGained => {}
        Event::FocusLost => {
            clear_hover(state);
        }
        Event::Paste(text) => {
            if state
                .git_view
                .as_ref()
                .is_some_and(|view| view.focus == GitFocus::Commit && view.confirm.is_none())
                && let Some(view) = state.git_view.as_mut()
            {
                let clean = text.replace('\n', " ");
                commit_insert(view, &clean);
            }
        }
        Event::MouseScroll(amount) => {
            let Some(view) = state.git_view.as_mut() else {
                return;
            };
            if view.confirm.is_some() {
                return;
            }
            // Wheel-up (`amount > 0`, button 64) moves toward the top
            // (earlier rows); wheel-down moves toward the bottom. All git
            // scroll offsets count from the top, so positive amounts shrink
            // the offset. (The main transcript counts from the bottom, which
            // is why its sign looks reversed.)
            let over_history = view.mouse_position.is_some_and(|point| {
                point.column > view.layout.divider
                    && point.column <= view.layout.divider + view.layout.right_width
                    && view
                        .layout
                        .history_rows
                        .iter()
                        .any(|(row, _)| *row == point.row)
            });
            let history_wheel =
                over_history || (view.mouse_position.is_none() && view.focus == GitFocus::History);
            if history_wheel {
                if amount > 0 {
                    view.history_scroll = view.history_scroll.saturating_sub(amount as usize);
                } else {
                    view.history_scroll = view
                        .history_scroll
                        .saturating_add(amount.unsigned_abs() as usize);
                }
                let visible = view.layout.history_rows.len();
                if visible > 0 {
                    view.history_scroll = history_top(view, visible);
                    view.history_selected = view
                        .history_selected
                        .clamp(view.history_scroll, view.history_scroll + visible - 1);
                }
            } else if view.show_log {
                if amount > 0 {
                    view.log_scroll = view.log_scroll.saturating_sub(amount as usize);
                } else {
                    view.log_scroll = view
                        .log_scroll
                        .saturating_add(amount.unsigned_abs() as usize);
                }
            } else if view.focus == GitFocus::Diff || view.diff.is_some() {
                if let Some(diff) = view.diff.as_mut() {
                    resolve_diff_scroll(diff);
                    if amount > 0 {
                        diff.scroll = diff.scroll.saturating_sub(amount as usize);
                    } else {
                        diff.scroll = diff.scroll.saturating_add(amount.unsigned_abs() as usize);
                    }
                }
            } else if amount > 0 {
                view.file_scroll = view.file_scroll.saturating_sub(amount as usize);
            } else {
                view.file_scroll = view
                    .file_scroll
                    .saturating_add(amount.unsigned_abs() as usize);
            }
            // Scrolling the history (or the full log) near the oldest loaded
            // commit pages in the next batch so the list can grow to the full
            // history without an upfront full-log load.
            if state.git_job.is_none() {
                maybe_load_more_history(state);
            }
        }
        Event::Mouse(mouse) => {
            handle_mouse(state, mouse);
        }
        Event::Key(key) => {
            clear_hover(state);
            handle_key(state, key);
        }
    }
}

pub(in crate::tui) fn clear_hover(state: &mut ViewState) -> bool {
    state
        .git_view
        .as_mut()
        .is_some_and(|view| view.mouse_position.take().is_some())
}

pub(in crate::tui) fn pointer_over_control(state: &ViewState) -> bool {
    state.git_view.as_ref().is_some_and(|view| {
        !matches!(
            view.mouse_target(),
            None | Some(MouseTarget::CommitInput | MouseTarget::DismissMenu)
        )
    })
}

pub(super) fn handle_mouse(state: &mut ViewState, mouse: MouseEvent) {
    let Some(view) = state.git_view.as_mut() else {
        return;
    };
    view.mouse_position = Some(ScreenPoint {
        row: mouse.row,
        column: mouse.column,
    });
    if mouse.kind != MouseKind::Press {
        return;
    }
    match view.mouse_target() {
        Some(MouseTarget::Confirm(true)) => confirm_action(state),
        Some(MouseTarget::Confirm(false)) => view.confirm = None,
        Some(MouseTarget::CloseDiff) => {
            view.diff = None;
            view.show_log = false;
            view.show_branches = false;
            view.focus = GitFocus::Files;
        }
        Some(MouseTarget::MenuItem(item)) => {
            if let Some(menu) = view.commit_menu.as_mut() {
                menu.selected = item;
            }
            activate_commit_menu(state);
        }
        Some(MouseTarget::Commit) => do_commit(state),
        Some(MouseTarget::StageAll) => {
            run_simple(state, &["add", "-A"], "Staged all files.");
        }
        Some(MouseTarget::DismissMenu) => view.commit_menu = None,
        Some(MouseTarget::ToggleMenu) => {
            view.commit_menu = Some(CommitMenuState::default());
        }
        Some(MouseTarget::FileAction(index, action)) => {
            view.selected = index;
            view.focus = GitFocus::Files;
            match action {
                FileAction::Stage(staged) => {
                    view.confirm = None;
                    set_file_staged(state, index, staged);
                }
                FileAction::Discard => {
                    if let Some(file) = view.flat.get(index) {
                        view.confirm = Some(Confirm::DiscardFile {
                            path: file.path.clone(),
                            renamed_from: file.renamed_from.clone(),
                            untracked: file.section == GitSection::Untracked,
                            staged: file.section == GitSection::Staged,
                        });
                    }
                }
            }
        }
        Some(MouseTarget::CommitInput) => {
            view.focus = GitFocus::Commit;
            view.confirm = None;
        }
        Some(MouseTarget::File(index)) => {
            view.selected = index;
            view.focus = GitFocus::Files;
            view.confirm = None;
            open_selected_diff(state);
        }
        Some(MouseTarget::History(index)) => {
            view.history_selected = index;
            view.focus = GitFocus::History;
            view.confirm = None;
            // Same one-click behaviour as changed files: selecting a commit
            // immediately shows its diff on the left.
            open_commit_diff(state);
        }
        None => {}
    }
}

pub(super) fn handle_key(state: &mut ViewState, key: Key) {
    // Confirmation overlay owns every key.
    if state
        .git_view
        .as_ref()
        .is_some_and(|view| view.confirm.is_some())
    {
        match key {
            Key::Enter => confirm_action(state),
            Key::Escape | Key::Ctrl('c') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.confirm = None;
                }
            }
            _ => {}
        }
        return;
    }

    // Branch picker overlay.
    if state
        .git_view
        .as_ref()
        .is_some_and(|view| view.show_branches)
    {
        match key {
            Key::Escape | Key::Ctrl('c') | Key::Char('b') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.show_branches = false;
                }
            }
            Key::Up | Key::Char('k') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.branch_selected = view.branch_selected.saturating_sub(1);
                }
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut() {
                    let max = view.branches.len().saturating_sub(1);
                    view.branch_selected = (view.branch_selected + 1).min(max);
                }
            }
            Key::Enter => {
                let (root, branch) = {
                    let Some(view) = state.git_view.as_ref() else {
                        return;
                    };
                    (
                        view.root.clone(),
                        view.branches.get(view.branch_selected).cloned(),
                    )
                };
                if let Some(branch) = branch {
                    jobs::start(state, "Running git…", move |state| {
                        match run_git(&root, &["switch", &branch]) {
                            Ok(_) => {
                                if let Some(view) = state.git_view.as_mut() {
                                    view.show_branches = false;
                                    view.set_notice(format!("Switched to {branch}."), false);
                                }
                                operations::refresh(state);
                            }
                            Err(error) => {
                                if let Some(view) = state.git_view.as_mut() {
                                    view.set_notice(error, true);
                                }
                            }
                        }
                    });
                }
            }
            Key::Char('n') => {
                let (root, name) = {
                    let Some(view) = state.git_view.as_ref() else {
                        return;
                    };
                    (view.root.clone(), view.commit.trim().to_string())
                };
                if name.is_empty() || name.contains(char::is_whitespace) {
                    if let Some(view) = state.git_view.as_mut() {
                        view.set_notice(
                            "Type a branch name in the message box, then press n.".to_string(),
                            true,
                        );
                    }
                    return;
                }
                jobs::start(state, "Running git…", move |state| {
                    match run_git(&root, &["switch", "-c", &name]) {
                        Ok(_) => {
                            if let Some(view) = state.git_view.as_mut() {
                                view.show_branches = false;
                                view.commit.clear();
                                view.commit_cursor = 0;
                                view.set_notice(format!("Created branch {name}."), false);
                            }
                            operations::refresh(state);
                        }
                        Err(error) => {
                            if let Some(view) = state.git_view.as_mut() {
                                view.set_notice(error, true);
                            }
                        }
                    }
                });
            }
            _ => {}
        }
        return;
    }

    // Commit-options dropdown owns navigation while open. Any other key
    // dismisses it and falls through to normal handling, so typing a commit
    // message just works.
    if state
        .git_view
        .as_ref()
        .is_some_and(|view| view.commit_menu.is_some())
    {
        match key {
            Key::Up | Key::Char('k') => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(menu) = view.commit_menu.as_mut()
                {
                    menu.selected = menu.selected.saturating_sub(1);
                }
                return;
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(menu) = view.commit_menu.as_mut()
                {
                    menu.selected = (menu.selected + 1).min(MENU_ITEMS - 1);
                }
                return;
            }
            Key::Enter => {
                activate_commit_menu(state);
                return;
            }
            Key::Escape | Key::Tab | Key::Ctrl('c') | Key::Char('v') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.commit_menu = None;
                }
                return;
            }
            _ => {
                if let Some(view) = state.git_view.as_mut() {
                    view.commit_menu = None;
                }
            }
        }
    }

    // Close the visible diff in one step, regardless of which panel has focus.
    if matches!(key, Key::Escape | Key::Ctrl('c'))
        && let Some(view) = state.git_view.as_mut()
        && view.diff.is_some()
    {
        view.diff = None;
        view.focus = GitFocus::Files;
        return;
    }

    let focus = state.git_view.as_ref().map(|view| view.focus);
    match focus {
        Some(GitFocus::Commit) => match key {
            Key::Escape | Key::Ctrl('c') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Files;
                }
            }
            Key::Enter | Key::Ctrl('g') => do_commit(state),
            Key::Tab => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = if view.diff.is_some() {
                        GitFocus::Diff
                    } else {
                        GitFocus::Files
                    };
                }
            }
            key => {
                if let Some(view) = state.git_view.as_mut() {
                    let _ = commit_key(view, key);
                }
            }
        },
        Some(GitFocus::Diff) => match key {
            Key::Escape | Key::Ctrl('c') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Files;
                }
            }
            Key::Tab => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Files;
                }
            }
            Key::Up | Key::Char('k') => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    resolve_diff_scroll(diff);
                    diff.scroll = diff.scroll.saturating_sub(1);
                }
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    resolve_diff_scroll(diff);
                    diff.scroll = diff.scroll.saturating_add(1);
                }
            }
            Key::PageUp => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    resolve_diff_scroll(diff);
                    diff.scroll = diff.scroll.saturating_sub(10);
                }
            }
            Key::PageDown => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    resolve_diff_scroll(diff);
                    diff.scroll = diff.scroll.saturating_add(10);
                }
            }
            Key::Enter => {
                if let Some(view) = state.git_view.as_mut() {
                    view.diff = None;
                    view.focus = GitFocus::Files;
                }
            }
            Key::Char(' ') => toggle_stage_selected(state),
            _ => {}
        },
        Some(GitFocus::History) => match key {
            Key::Escape | Key::Ctrl('c') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Files;
                }
            }
            Key::Tab => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Commit;
                }
            }
            Key::Up | Key::Char('k') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.history_selected = view.history_selected.saturating_sub(1);
                }
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut()
                    && !view.history.is_empty()
                {
                    view.history_selected = (view.history_selected + 1).min(view.history.len() - 1);
                }
                if state.git_job.is_none() {
                    maybe_load_more_history(state);
                }
            }
            Key::PageUp => {
                if let Some(view) = state.git_view.as_mut() {
                    view.history_selected = view.history_selected.saturating_sub(5);
                }
            }
            Key::PageDown => {
                if let Some(view) = state.git_view.as_mut()
                    && !view.history.is_empty()
                {
                    view.history_selected = (view.history_selected + 5).min(view.history.len() - 1);
                }
                if state.git_job.is_none() {
                    maybe_load_more_history(state);
                }
            }
            Key::Enter | Key::Right | Key::Char('l') => open_commit_diff(state),
            _ => {}
        },
        Some(GitFocus::Files) | None => match key {
            Key::Escape | Key::Ctrl('c') => {
                let (has_diff, show_log) = state
                    .git_view
                    .as_ref()
                    .map(|view| (view.diff.is_some(), view.show_log))
                    .unwrap_or((false, false));
                if has_diff {
                    if let Some(view) = state.git_view.as_mut() {
                        view.diff = None;
                    }
                } else if show_log {
                    if let Some(view) = state.git_view.as_mut() {
                        view.show_log = false;
                    }
                } else {
                    state.git_view = None;
                }
            }
            Key::Tab => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::History;
                }
            }
            Key::Up | Key::Char('k') => {
                if let Some(view) = state.git_view.as_mut() {
                    if view.show_log {
                        view.log_scroll = view.log_scroll.saturating_sub(1);
                    } else {
                        view.selected = view.selected.saturating_sub(1);
                    }
                }
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut() {
                    if view.show_log {
                        view.log_scroll = view.log_scroll.saturating_add(1);
                    } else if !view.flat.is_empty() {
                        view.selected = (view.selected + 1).min(view.flat.len() - 1);
                    }
                }
                if state.git_job.is_none() {
                    maybe_load_more_history(state);
                }
            }
            Key::PageUp => {
                if let Some(view) = state.git_view.as_mut() {
                    if view.show_log {
                        view.log_scroll = view.log_scroll.saturating_sub(10);
                    } else {
                        view.selected = view.selected.saturating_sub(10);
                    }
                }
            }
            Key::PageDown => {
                if let Some(view) = state.git_view.as_mut() {
                    if view.show_log {
                        view.log_scroll = view.log_scroll.saturating_add(10);
                    } else if !view.flat.is_empty() {
                        view.selected = (view.selected + 10).min(view.flat.len().saturating_sub(1));
                    }
                }
                if state.git_job.is_none() {
                    maybe_load_more_history(state);
                }
            }
            Key::Enter | Key::Right => {
                if state
                    .git_view
                    .as_ref()
                    .is_some_and(|view| !view.show_log && !view.flat.is_empty())
                {
                    open_selected_diff(state);
                }
            }
            Key::Left | Key::Char('h') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.diff = None;
                }
            }
            Key::Char(' ') => {
                if state.git_view.as_ref().is_some_and(|view| !view.show_log) {
                    toggle_stage_selected(state);
                }
            }
            Key::Char('+') => {
                if let Some(index) = state.git_view.as_ref().and_then(|view| {
                    (!view.show_log && !view.flat.is_empty()).then_some(view.selected)
                }) {
                    set_file_staged(state, index, true);
                }
            }
            Key::Char('-') => {
                if let Some(index) = state.git_view.as_ref().and_then(|view| {
                    (!view.show_log && !view.flat.is_empty()).then_some(view.selected)
                }) {
                    set_file_staged(state, index, false);
                }
            }
            Key::Char('a') => run_simple(state, &["add", "-A"], "Staged all files."),
            Key::Char('u') => run_simple(state, &["reset"], "Unstaged all files."),
            Key::Char('e') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.focus = GitFocus::Commit;
                }
            }
            Key::Char('C') => do_commit(state),
            Key::Char('m') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.amend = !view.amend;
                }
            }
            Key::Char('v') => {
                if let Some(view) = state.git_view.as_mut() {
                    if view.commit_menu.is_some() {
                        view.commit_menu = None;
                    } else {
                        view.commit_menu = Some(CommitMenuState { selected: 0 });
                    }
                }
            }
            Key::Char('d') => {
                let file = state
                    .git_view
                    .as_ref()
                    .and_then(|view| view.selected_file().cloned());
                if let Some(file) = file {
                    let untracked = file.section == GitSection::Untracked;
                    let staged = file.section == GitSection::Staged;
                    if let Some(view) = state.git_view.as_mut() {
                        view.confirm = Some(Confirm::DiscardFile {
                            path: file.path,
                            renamed_from: file.renamed_from,
                            untracked,
                            staged,
                        });
                    }
                }
            }
            Key::Char('D') => {
                if let Some(view) = state.git_view.as_mut() {
                    view.confirm = Some(Confirm::DiscardAll);
                }
            }
            Key::Char('p') => run_simple(state, &["push"], "Pushed."),
            Key::Char('f') => run_simple(state, &["fetch"], "Fetched."),
            Key::Char('F') => run_simple(state, &["pull", "--ff-only"], "Pulled."),
            Key::Char('s') => {
                let message = state
                    .git_view
                    .as_ref()
                    .map(|view| view.commit.trim().to_string())
                    .unwrap_or_default();
                if message.is_empty() {
                    run_simple(state, &["stash", "push"], "Stashed.");
                } else {
                    let root = state.git_view.as_ref().map(|view| view.root.clone());
                    if let Some(root) = root {
                        jobs::start(state, "Running git…", move |state| {
                            match run_git(&root, &["stash", "push", "-m", &message]) {
                                Ok(_) => {
                                    if let Some(view) = state.git_view.as_mut() {
                                        view.commit.clear();
                                        view.commit_cursor = 0;
                                        view.set_notice("Stashed.".to_string(), false);
                                    }
                                    operations::refresh(state);
                                }
                                Err(error) => {
                                    if let Some(view) = state.git_view.as_mut() {
                                        view.set_notice(error, true);
                                    }
                                }
                            }
                        });
                    }
                }
            }
            Key::Char('S') => run_simple(state, &["stash", "pop"], "Restored stash."),
            Key::Char('l') => {
                let turning_on = state.git_view.as_ref().is_some_and(|view| !view.show_log);
                if turning_on {
                    let (root, limit) = state
                        .git_view
                        .as_ref()
                        .map(|view| (view.root.clone(), view.history_limit))
                        .unwrap_or((PathBuf::new(), INITIAL_HISTORY_LIMIT));
                    if !root.as_os_str().is_empty() {
                        let (history, has_more) = load_history_limit(&root, limit);
                        if let Some(view) = state.git_view.as_mut() {
                            view.show_log = true;
                            view.history = history;
                            view.history_has_more = has_more;
                            view.history_selected = 0;
                            view.log_scroll = 0;
                            view.diff = None;
                            view.show_branches = false;
                        }
                    }
                } else if let Some(view) = state.git_view.as_mut() {
                    view.show_log = false;
                }
            }
            Key::Char('b') => {
                let root = state.git_view.as_ref().map(|view| view.root.clone());
                if let Some(root) = root {
                    jobs::start(
                        state,
                        "Loading branches…",
                        move |state| match load_branches(&root) {
                            Ok(branches) => {
                                if let Some(view) = state.git_view.as_mut() {
                                    view.show_branches = true;
                                    view.branches = branches;
                                    view.branch_selected = 0;
                                    view.diff = None;
                                    view.show_log = false;
                                }
                            }
                            Err(error) => {
                                if let Some(view) = state.git_view.as_mut() {
                                    view.set_notice(error, true);
                                }
                            }
                        },
                    );
                }
            }
            Key::Char('r') => refresh(state),
            _ => {}
        },
    }
}

/// Routes `Ctrl+C`/`Esc` SIGINT-equivalents to the git view. Returns whether
/// the view consumed the interrupt (used by the idle event loop).
pub(in crate::tui) fn handle_interrupt(state: &mut ViewState) -> bool {
    if jobs::cancel(state) {
        return true;
    }
    if state.git_init.is_some() {
        state.git_init = None;
        return true;
    }
    if state.git_view.is_none() {
        return false;
    }
    handle_event(state, &mut Editor::default(), Event::Key(Key::Escape));
    true
}
