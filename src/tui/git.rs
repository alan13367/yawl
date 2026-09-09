//! Git dashboard: right-docked CHANGES panel plus a diff view.
//!
//! `/git` opens a split takeover. The right card contains an inline commit
//! message row, a Commit button with an amend toggle, then staged /
//! unstaged file groups. Selecting a file (Enter or click) swaps the left
//! pane from the chat transcript to a
//! unified diff with old/new line numbers and syntax-highlighted code. `Esc`
//! or the `✕` header returns to the file list; a further `Esc` closes the
//! panel back to chat.
//!
//! The backend shells out to the system `git` binary only (no new
//! dependencies) with non-interactive env so pushes never hang on credential
//! prompts.
//!
//! This facade owns shared dashboard state and refresh coordination. Private
//! children handle repository I/O, background jobs, input, initialization,
//! panel rendering, and diff rendering. TUI callers keep the entry points here.

mod diff;
mod init;
mod input;
pub(super) mod jobs;
mod operations;
mod render;
mod repository;

pub(super) use init::{GitInitFlow, handle_init_event, render_init};
pub(super) use input::{clear_hover, handle_event, handle_interrupt, pointer_over_control};
pub(super) use render::render;
use repository::load_diff;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::ViewState;
use super::terminal::ScreenPoint;

/// How often an open dashboard re-reads `git status` so changes made outside
/// Yawl (another terminal, an editor) appear without reopening. The raw
/// output is fingerprinted first. The visible worktree/index diff is also
/// checked because porcelain does not fingerprint contents. Unchanged data
/// avoids redraws; refreshes preserve selection, the commit box, and scroll.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// History pagination: the dashboard loads only the newest batch up front so
/// opening stays fast on large repositories, then pages in older commits as
/// the history (or full log) scrolls near the oldest loaded row.
const INITIAL_HISTORY_LIMIT: usize = 100;

const HISTORY_PAGE_SIZE: usize = 100;

/// How close to the oldest loaded commit (in rows) triggers the next page.
const HISTORY_PRELOAD_THRESHOLD: usize = 10;

/// Fixed right-panel chrome rows: title, three-row message box, commit
/// button, meta, notice, blank.
const FIXED_ROWS: usize = 8;

/// First frame row of the message box (top border); the input sits one row
/// below and the bottom border one row further down.
const COMMIT_BOX_TOP: usize = 1;

const COMMIT_BOX_INPUT: usize = COMMIT_BOX_TOP + 1;

/// Height of the message box in rows (top border, input, bottom border).
const COMMIT_BOX_HEIGHT: usize = 3;

/// HISTORY chrome: header row plus the two-line detail footer.
const HISTORY_CHROME_ROWS: usize = 3;

/// Minimum visible history entries when the panel must share space with a
/// long file list. Small terminals may still shrink below this.
const MIN_HISTORY_ENTRIES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GitFocus {
    Files,
    History,
    Commit,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitSection {
    Staged,
    Unstaged,
    Untracked,
}

impl GitSection {
    fn title(self) -> &'static str {
        match self {
            Self::Staged => "STAGED",
            Self::Unstaged => "UNSTAGED",
            Self::Untracked => "UNTRACKED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitFile {
    /// Repo-relative display path.
    path: String,
    /// Index status code (`X` from porcelain).
    x: char,
    /// Worktree status code (`Y` from porcelain).
    y: char,
    section: GitSection,
    renamed_from: Option<String>,
}

impl GitFile {
    fn status_label(&self) -> String {
        match self.section {
            GitSection::Staged => self.x.to_string(),
            GitSection::Unstaged => {
                if self.x == 'U'
                    || self.y == 'U'
                    || (self.x == 'A' && self.y == 'A')
                    || (self.x == 'D' && self.y == 'D')
                {
                    "UU".to_string()
                } else {
                    self.y.to_string()
                }
            }
            GitSection::Untracked => "U".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GitStatus {
    branch: String,
    upstream: Option<String>,
    ahead: usize,
    behind: usize,
    staged: Vec<GitFile>,
    unstaged: Vec<GitFile>,
    untracked: Vec<GitFile>,
}

impl GitStatus {
    fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    fn flat(&self) -> Vec<GitFile> {
        let mut flat = Vec::new();
        flat.extend(self.staged.iter().cloned());
        flat.extend(self.unstaged.iter().cloned());
        flat.extend(self.untracked.iter().cloned());
        flat
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Added,
    Removed,
    Hunk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffLine {
    old_no: Option<usize>,
    new_no: Option<usize>,
    kind: DiffKind,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadedDiff {
    path: String,
    staged: bool,
    untracked: bool,
    /// Full commit hash when the diff shows a historical commit rather than
    /// the worktree. Such diffs are never reloaded by `refresh`.
    commit: Option<String>,
    lines: Vec<DiffLine>,
    added: usize,
    removed: usize,
    scroll: usize,
    binary: bool,
    truncated: bool,
}

/// One row of the HISTORY section below the changed files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct HistoryEntry {
    hash: String,
    short: String,
    refs: String,
    author: String,
    date: String,
    subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    DiscardFile {
        path: String,
        renamed_from: Option<String>,
        untracked: bool,
        staged: bool,
    },
    DiscardAll,
}

/// Modal confirm box for destructive undo actions. Anchored over the file
/// area so it cannot be missed; exactly `CONFIRM_HEIGHT` rows.
const CONFIRM_TOP: usize = FIXED_ROWS;

const CONFIRM_HEIGHT: usize = 4;

/// Clickable answer inside the confirm box. Columns are zero-based screen
/// coordinates; `x1` is exclusive.
#[derive(Debug, Clone, Copy)]
struct ConfirmHit {
    row: usize,
    x0: usize,
    x1: usize,
    confirm: bool,
}

#[derive(Debug, Clone, Default)]
struct GitLayout {
    divider: usize,
    height: usize,
    file_rows: Vec<(usize, usize)>,
    actions: Vec<FileActionHit>,
    stage_all_row: Option<usize>,
    history_rows: Vec<(usize, usize)>,
    menu_rows: Vec<(usize, usize)>,
    confirm_rows: Vec<ConfirmHit>,
    commit_row: usize,
    commit_box_top: usize,
    commit_x0: usize,
    commit_x1: usize,
    commit_button_row: usize,
    commit_button_x0: usize,
    commit_button_x1: usize,
    menu_button_x0: usize,
    menu_button_x1: usize,
    close_button: Option<(usize, usize)>,
    right_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseTarget {
    File(usize),
    FileAction(usize, FileAction),
    StageAll,
    History(usize),
    CommitInput,
    Commit,
    ToggleMenu,
    MenuItem(usize),
    DismissMenu,
    Confirm(bool),
    CloseDiff,
}

impl GitLayout {
    fn target(&self, point: ScreenPoint) -> Option<MouseTarget> {
        let ScreenPoint { row, column } = point;
        if !self.confirm_rows.is_empty() {
            return self.confirm_rows.iter().find_map(|hit| {
                (hit.row == row && (hit.x0..hit.x1).contains(&column))
                    .then_some(MouseTarget::Confirm(hit.confirm))
            });
        }
        if let Some((close_row, close_column)) = self.close_button
            && row == close_row
            && (close_column..close_column + 3).contains(&column)
        {
            return Some(MouseTarget::CloseDiff);
        }
        if !(self.divider + 1..self.divider + 1 + self.right_width).contains(&column) {
            return None;
        }
        let commit = row == self.commit_button_row
            && (self.commit_button_x0..self.commit_button_x1).contains(&column);
        if !self.menu_rows.is_empty() {
            return Some(
                self.menu_rows
                    .iter()
                    .find_map(|(menu_row, item)| {
                        (*menu_row == row).then_some(MouseTarget::MenuItem(*item))
                    })
                    .unwrap_or(if commit {
                        MouseTarget::Commit
                    } else {
                        MouseTarget::DismissMenu
                    }),
            );
        }
        if row == self.commit_button_row
            && (self.menu_button_x0..self.menu_button_x1).contains(&column)
        {
            return Some(MouseTarget::ToggleMenu);
        }
        // The whole message box focuses the input, borders included.
        if (self.commit_box_top..self.commit_box_top + COMMIT_BOX_HEIGHT).contains(&row)
            && (self.commit_x0..self.commit_x1).contains(&column)
        {
            return Some(MouseTarget::CommitInput);
        }
        if let Some(hit) = self
            .actions
            .iter()
            .find(|hit| hit.row == row && (hit.x0..hit.x1).contains(&column))
        {
            return Some(MouseTarget::FileAction(hit.index, hit.action));
        }
        if self.stage_all_row == Some(row)
            && column >= self.divider + 1 + self.right_width.saturating_sub(3)
        {
            return Some(MouseTarget::StageAll);
        }
        if commit {
            return Some(MouseTarget::Commit);
        }
        self.file_rows
            .iter()
            .find_map(|(file_row, index)| (*file_row == row).then_some(MouseTarget::File(*index)))
            .or_else(|| {
                self.history_rows.iter().find_map(|(history_row, index)| {
                    (*history_row == row).then_some(MouseTarget::History(*index))
                })
            })
    }
}

/// Clickable per-file affordance at the right edge of a file row. Columns are
/// zero-based screen coordinates; `x1` is exclusive.
#[derive(Debug, Clone, Copy)]
struct FileActionHit {
    row: usize,
    x0: usize,
    x1: usize,
    index: usize,
    action: FileAction,
}

/// Per-file row actions: stage/unstage (`+` / `-`) and discard/undo (`↩`).
/// The undo cell always asks for confirmation first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileAction {
    Stage(bool),
    Discard,
}

#[derive(Clone)]
pub(super) struct GitView {
    root: PathBuf,
    status: GitStatus,
    flat: Vec<GitFile>,
    selected: usize,
    /// Offset in rendered rows, including section headers.
    file_scroll: usize,
    focus: GitFocus,
    commit: String,
    commit_cursor: usize,
    amend: bool,
    diff: Option<LoadedDiff>,
    show_log: bool,
    log_scroll: usize,
    show_branches: bool,
    branches: Vec<String>,
    branch_selected: usize,
    /// Commits listed in the HISTORY section below the changed files.
    history: Vec<HistoryEntry>,
    history_selected: usize,
    history_scroll: usize,
    /// How many newest commits are loaded (`git log --max-count`). Grows by
    /// [`HISTORY_PAGE_SIZE`] as the user scrolls toward older history.
    history_limit: usize,
    /// True when the last load hit the limit, so older commits may remain.
    history_has_more: bool,
    confirm: Option<Confirm>,
    notice: String,
    notice_error: bool,
    /// Raw status output backing the poll fingerprint and the instant of the
    /// last poll, so external changes surface without reopening the panel.
    last_status_raw: Option<String>,
    last_poll: Option<Instant>,
    /// Open commit-options dropdown (`∨`); `None` when closed.
    commit_menu: Option<CommitMenuState>,
    mouse_position: Option<ScreenPoint>,
    layout: GitLayout,
}

/// Selection within the commit-options dropdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CommitMenuState {
    selected: usize,
}

/// What a commit-button action does after the commit itself succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitMode {
    Commit,
    AndPush,
    AndSync,
}

/// First content row of the dropdown and its height (header + items +
/// footer). Anchored directly under the commit button.
const MENU_TOP: usize = COMMIT_BOX_TOP + COMMIT_BOX_HEIGHT + 1;

const MENU_ITEMS: usize = 4;

const MENU_HEIGHT: usize = MENU_ITEMS + 2;

impl GitView {
    fn mouse_target(&self) -> Option<MouseTarget> {
        self.mouse_position
            .and_then(|point| self.layout.target(point))
    }

    fn hovered_file(&self) -> Option<usize> {
        match self.mouse_target() {
            Some(MouseTarget::File(index) | MouseTarget::FileAction(index, _)) => Some(index),
            _ => None,
        }
    }

    fn hovered_history(&self) -> Option<usize> {
        match self.mouse_target() {
            Some(MouseTarget::History(index)) => Some(index),
            _ => None,
        }
    }

    fn new(root: PathBuf, status: GitStatus) -> Self {
        let flat = status.flat();
        Self {
            root,
            status,
            flat,
            selected: 0,
            file_scroll: 0,
            focus: GitFocus::Files,
            commit: String::new(),
            commit_cursor: 0,
            amend: false,
            diff: None,
            show_log: false,
            log_scroll: 0,
            show_branches: false,
            branches: Vec::new(),
            branch_selected: 0,
            history: Vec::new(),
            history_selected: 0,
            history_scroll: 0,
            history_limit: INITIAL_HISTORY_LIMIT,
            history_has_more: false,
            confirm: None,
            notice: String::new(),
            notice_error: false,
            last_status_raw: None,
            last_poll: None,
            commit_menu: None,
            mouse_position: None,
            layout: GitLayout::default(),
        }
    }

    fn selected_file(&self) -> Option<&GitFile> {
        self.flat.get(self.selected)
    }

    fn ensure_selection_visible(&mut self, visible: usize) {
        if self.flat.is_empty() {
            self.selected = 0;
            self.file_scroll = 0;
            return;
        }
        self.selected = self.selected.min(self.flat.len() - 1);
        if visible == 0 {
            return;
        }
        let rows = file_area_rows(self);
        let selected_row = rows
            .iter()
            .position(|row| matches!(row, FileAreaRow::File(index) if *index == self.selected))
            .unwrap_or(0);
        self.file_scroll = self.file_scroll.min(rows.len().saturating_sub(visible));
        if selected_row < self.file_scroll {
            self.file_scroll = selected_row;
        } else if selected_row >= self.file_scroll + visible {
            self.file_scroll = selected_row + 1 - visible;
        }
    }

    /// Keeps the selected history entry inside its own scroll window.
    fn ensure_history_visible(&mut self, visible: usize) {
        if self.history.is_empty() {
            self.history_selected = 0;
            self.history_scroll = 0;
            return;
        }
        self.history_selected = self.history_selected.min(self.history.len() - 1);
        if visible == 0 {
            return;
        }
        self.history_scroll = self
            .history_scroll
            .min(self.history.len().saturating_sub(visible));
        if self.history_selected < self.history_scroll {
            self.history_scroll = self.history_selected;
        } else if self.history_selected >= self.history_scroll + visible {
            self.history_scroll = self.history_selected + 1 - visible;
        }
    }

    fn set_notice(&mut self, text: String, error: bool) {
        self.notice = text;
        self.notice_error = error;
    }
}

/// One screen row of the file area: a group header or a file (flat index).
enum FileAreaRow {
    Header(GitSection),
    File(usize),
}

/// Ordered file-area rows (group headers + files) before scrolling.
fn file_area_rows(view: &GitView) -> Vec<FileAreaRow> {
    // One row per file plus at most one header per non-empty section.
    let mut rows = Vec::with_capacity(view.flat.len() + 2);
    let mut section = None;
    for (index, file) in view.flat.iter().enumerate() {
        let group = match file.section {
            GitSection::Untracked => GitSection::Unstaged,
            section => section,
        };
        if section != Some(group) {
            rows.push(FileAreaRow::Header(group));
            section = Some(group);
        }
        rows.push(FileAreaRow::File(index));
    }
    rows
}

pub(super) fn open_dashboard(state: &mut ViewState) {
    if state.git_job.is_some() {
        return;
    }
    state.picker = None;
    state.subagent_view = None;
    state.process_view = None;
    state.transcript.blur();
    state.scroll_offset = 0;
    jobs::start(state, "Opening git…", operations::open_dashboard);
}

pub(super) fn refresh(state: &mut ViewState) {
    jobs::start(state, "Refreshing…", operations::refresh);
}

/// Reloads the visible worktree/index diff without disturbing its scroll.
/// Historical diffs are immutable. Failed reads leave the previous diff intact.
fn refresh_visible_diff(view: &mut GitView) -> Result<bool, String> {
    let Some(previous) = view.diff.as_ref().filter(|diff| diff.commit.is_none()) else {
        return Ok(false);
    };
    let file = view.flat.iter().find(|file| {
        file.path == previous.path && (file.section == GitSection::Staged) == previous.staged
    });
    let Some(file) = file else {
        view.diff = None;
        if view.focus == GitFocus::Diff {
            view.focus = GitFocus::Files;
        }
        return Ok(true);
    };
    let mut diff = load_diff(&view.root, file)?;
    diff.scroll = previous.scroll;
    if &diff == previous {
        return Ok(false);
    }
    view.diff = Some(diff);
    Ok(true)
}

/// Re-reads `git status` when the poll interval has elapsed and refreshes the
/// open dashboard if the worktree changed elsewhere. Returns true when the
/// frame needs redrawing. Failures stay silent — a transient index lock from
/// an external git process must neither clobber notices nor spin the UI;
/// the next interval retries.
pub(super) fn poll_tick(state: &mut ViewState) -> bool {
    let changed = jobs::poll(state);
    if state.git_job.is_some() {
        return changed;
    }
    let Some(view) = state.git_view.as_mut() else {
        return changed;
    };
    if view
        .last_poll
        .is_some_and(|polled| polled.elapsed() < POLL_INTERVAL)
    {
        return changed;
    }
    view.last_poll = Some(Instant::now());
    jobs::start(state, "", operations::poll);
    changed
}

/// Truncates plain text to a visible width without padding.
fn truncate_visible(text: &str, width: usize) -> String {
    crate::tui::markdown::split_chars(text, width.max(1))
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn sanitize_plain(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' {
                ' '
            } else if c.is_control() {
                '�'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "git_tests.rs"]
mod regression_tests;
