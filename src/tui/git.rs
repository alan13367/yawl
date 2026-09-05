//! Git dashboard: right-docked CHANGES panel plus a diff view.
//!
//! `/git` opens a split takeover. The right card mirrors the requested
//! CHANGES design: an inline commit message row, a Commit button with an
//! amend toggle, then staged / unstaged / untracked file groups. Selecting a
//! file (Enter or click) swaps the left pane from the chat transcript to a
//! unified diff with old/new line numbers and syntax-highlighted code. `Esc`
//! or the `✕` header returns to the file list; a further `Esc` closes the
//! panel back to chat.
//!
//! The backend shells out to the system `git` binary only (no new
//! dependencies) with non-interactive env so pushes never hang on credential
//! prompts.

pub(super) mod jobs;
mod operations;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::UiColor;

use super::events::{Event, Key, MouseEvent, MouseKind};
use super::input::Editor;
use super::render::{
    HIDDEN_CURSOR, ImageSupport, foreground_color, selected_row, selection_style, status_style,
};
use super::terminal::ScreenPoint;
use super::{ViewState, markdown};

const MAX_DIFF_LINES: usize = 2000;
const MAX_DIFF_BYTES: usize = 1024 * 1024;

/// How often an open dashboard re-reads `git status` so changes made outside
/// Yawl (another terminal, an editor) appear without reopening. The raw
/// output is fingerprinted first. The visible worktree/index diff is also
/// checked because porcelain does not fingerprint contents. Unchanged data
/// avoids redraws; refreshes preserve selection, the commit box, and scroll.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// `git status` invocation shared by loads and the poll fingerprint.
const STATUS_ARGS: &[&str] = &[
    "status",
    "--porcelain=v1",
    "-z",
    "-b",
    "--untracked-files=normal",
];

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
            GitSection::Untracked => "??".to_string(),
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
    /// Short hash when the diff shows a historical commit rather than the
    /// worktree. Such diffs are never reloaded by `refresh`.
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

/// Modal confirm box for destructive undo actions. Anchored over the file
/// area so it cannot be missed; exactly `CONFIRM_HEIGHT` rows.
const CONFIRM_TOP: usize = 6;
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
    file_rows: Vec<(usize, usize)>,
    actions: Vec<FileActionHit>,
    history_rows: Vec<(usize, usize)>,
    menu_rows: Vec<(usize, usize)>,
    confirm_rows: Vec<ConfirmHit>,
    commit_row: usize,
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
        if let Some(hit) = self
            .actions
            .iter()
            .find(|hit| hit.row == row && (hit.x0..hit.x1).contains(&column))
        {
            return Some(MouseTarget::FileAction(hit.index, hit.action));
        }
        if row == self.commit_row && (self.commit_x0..self.commit_x1).contains(&column) {
            return Some(MouseTarget::CommitInput);
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

/// First content row of the dropdown and its height (header + items +
/// footer). Anchored directly under the commit button.
const MENU_TOP: usize = 3;
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

// ---------------------------------------------------------------------------
// Backend: blocking `git` invocations, non-interactive.
// ---------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// How locating the work-tree root can fail. The variants matter: a plain
/// non-repo offers repository setup, while a git failure is shown as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FindRootError {
    /// The directory is not inside a git work tree.
    NotARepo,
    /// Git itself failed (missing binary, I/O error, empty root).
    Git(String),
}

impl std::fmt::Display for FindRootError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotARepo => formatter.write_str("not a git repository"),
            Self::Git(detail) => formatter.write_str(detail),
        }
    }
}

/// Resolves the work-tree root containing `cwd` via `git rev-parse`.
///
/// # Errors
///
/// Returns [`FindRootError::NotARepo`] when `cwd` is not inside a work tree.
/// Returns [`FindRootError::Git`] when git is missing, cannot run, or prints
/// an empty root.
fn find_root(cwd: &Path) -> Result<PathBuf, FindRootError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            FindRootError::Git(if error.kind() == std::io::ErrorKind::NotFound {
                "git is not installed".to_string()
            } else {
                format!("could not run git: {error}")
            })
        })?;
    if !output.status.success() {
        return Err(FindRootError::NotARepo);
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Err(FindRootError::Git(
            "git returned an empty work-tree root".to_string(),
        ));
    }
    Ok(PathBuf::from(root))
}

/// Maximum interval between cancellation checks; completion wakes immediately.
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Runs `git -C root …` non-interactively and returns its stdout.
fn run_git(root: &Path, args: &[&str]) -> Result<String, String> {
    run_git_env(root, args, &[])
}

/// Runs `git -C root …` non-interactively and returns stdout. Extra `env`
/// entries extend the child environment (tests pin `GIT_CONFIG_GLOBAL`
/// this way) without touching the process environment.
///
/// A waiter reports child completion while the worker checks cancellation.
/// Cancellation kills and reaps the process group, including hooks holding
/// output pipes open, without changing the caller's cancellation flag.
///
/// # Errors
///
/// Returns a message when git is missing, cannot spawn, is cancelled, or
/// exits non-zero (carrying the first line of its trimmed stderr, falling
/// back to stdout).
fn run_git_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    run_git_output(root, args, env, usize::MAX)
}

/// Captures at most `stdout_limit` bytes while continuing to drain the child.
fn run_git_output(
    root: &Path,
    args: &[&str],
    env: &[(&str, &str)],
    stdout_limit: usize,
) -> Result<String, String> {
    if crate::cancellation::interrupted() {
        return Err("cancelled.".into());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C")
        .envs(env.iter().copied())
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "git is not installed".to_string()
            } else {
                format!("could not run git: {error}")
            }
        })?;
    // Drain stdout/stderr on reader threads while polling: a child that
    // emits more than the pipe buffer (~64 KiB, e.g. `diff -U10000` of a
    // large file) would otherwise block on write() forever while we block
    // on try_wait().
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let pid = child.id() as libc::pid_t;
    let (status, stdout_bytes, stderr_bytes) = std::thread::scope(|scope| {
        let out_handle = scope.spawn(|| drain_pipe(stdout_pipe, stdout_limit));
        let err_handle = scope.spawn(|| drain_pipe(stderr_pipe, usize::MAX));
        // A blocking waiter reports completion immediately. The timeout only
        // bounds cancellation checks, so fast Git commands do not pay a tick.
        let (tx, rx) = std::sync::mpsc::channel();
        scope.spawn(move || {
            let _ = tx.send(child.wait());
        });
        let status = loop {
            if crate::cancellation::interrupted() {
                kill_git_group(pid);
                return Err("cancelled.".to_string());
            }
            match rx.recv_timeout(GIT_POLL_INTERVAL) {
                Ok(Ok(status)) => break status,
                Ok(Err(error)) => {
                    kill_git_group(pid);
                    return Err(format!("could not wait for git: {error}"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    kill_git_group(pid);
                    return Err("Git waiter stopped unexpectedly".into());
                }
            }
        };
        let stdout_bytes = out_handle.join().unwrap_or_default();
        let stderr_bytes = err_handle.join().unwrap_or_default();
        Ok::<_, String>((status, stdout_bytes, stderr_bytes))
    })?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let first = detail.lines().next().unwrap_or("git failed").trim();
        return Err(format!("git {} failed: {first}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&stdout_bytes).into_owned())
}

fn kill_git_group(pid: libc::pid_t) {
    // SAFETY: the child was started in its own process group; a negative PID
    // targets that group, including hooks that inherited the output pipes.
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

/// Drains a piped child stream to completion. `None` (pipe already taken)
/// yields empty output.
fn drain_pipe(pipe: Option<impl std::io::Read>, limit: usize) -> Vec<u8> {
    let Some(mut pipe) = pipe else {
        return Vec::new();
    };
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    loop {
        match std::io::Read::read(&mut pipe, &mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let keep = read.min(limit.saturating_sub(buffered.len()));
                buffered.extend_from_slice(&chunk[..keep]);
            }
        }
    }
    buffered
}

fn parse_branch_header(header: &str, status: &mut GitStatus) {
    // `## main...origin/main [ahead 1, behind 2]`, `## main`, `## No commits
    // yet on main`, `## HEAD (no branch)`.
    let header = header.strip_prefix("## ").unwrap_or(header);
    let (branch_part, extra) = match header.split_once(" [") {
        Some((left, right)) => (left, Some(right)),
        None => (header, None),
    };
    if let Some((local, upstream)) = branch_part.split_once("...") {
        status.branch = if local.is_empty() {
            "(detached)".to_string()
        } else {
            local.to_string()
        };
        status.upstream = if upstream.is_empty() {
            None
        } else {
            Some(upstream.to_string())
        };
    } else if branch_part == "HEAD (no branch)" {
        status.branch = "(detached)".to_string();
    } else {
        status.branch = branch_part
            .strip_prefix("No commits yet on ")
            .unwrap_or(branch_part)
            .to_string();
    }
    if let Some(extra) = extra {
        for chunk in extra.trim_end_matches(']').split(", ") {
            if let Some(count) = chunk.strip_prefix("ahead ") {
                status.ahead = count.parse().unwrap_or(0);
            } else if let Some(count) = chunk.strip_prefix("behind ") {
                status.behind = count.parse().unwrap_or(0);
            }
        }
    }
}

fn parse_status_porcelain(output: &str) -> GitStatus {
    let mut status = GitStatus {
        branch: "(unknown)".to_string(),
        ..GitStatus::default()
    };
    let mut tokens = output.split_terminator('\0').peekable();
    if let Some(first) = tokens.peek()
        && first.starts_with("## ")
    {
        let header = tokens.next().unwrap_or_default().to_string();
        parse_branch_header(&header, &mut status);
    }
    let mut pending_rename: Option<(char, char)> = None;
    let mut pending_path: Option<String> = None;
    for token in tokens {
        if pending_path.is_none() && pending_rename.is_none() {
            if token.len() < 4 {
                continue;
            }
            let mut chars = token.chars();
            let x = chars.next().unwrap_or(' ');
            let y = chars.next().unwrap_or(' ');
            if token.as_bytes().get(2) != Some(&b' ') {
                continue;
            }
            let path = token[3..].to_string();
            if x == 'R' || x == 'C' {
                pending_rename = Some((x, y));
                pending_path = Some(path);
                continue;
            }
            push_status_entry(&mut status, x, y, path, None);
        } else {
            // Second NUL of a rename/copy entry: the original path. Porcelain
            // -z reverses the order (new first, original second).
            let new_path = pending_path.take().unwrap_or_default();
            let (x, y) = pending_rename.take().unwrap_or(('R', ' '));
            push_status_entry(&mut status, x, y, new_path, Some(token.to_string()));
        }
    }
    status
}

fn push_status_entry(
    status: &mut GitStatus,
    x: char,
    y: char,
    path: String,
    renamed_from: Option<String>,
) {
    if x == '?' && y == '?' {
        status.untracked.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Untracked,
            renamed_from,
        });
        return;
    }
    if x == '!' {
        return;
    }
    let unmerged = x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D');
    if unmerged {
        status.unstaged.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Unstaged,
            renamed_from,
        });
        return;
    }
    if x != ' ' {
        status.staged.push(GitFile {
            path: path.clone(),
            x,
            y,
            section: GitSection::Staged,
            renamed_from: renamed_from.clone(),
        });
    }
    if y != ' ' {
        status.unstaged.push(GitFile {
            path,
            x,
            y,
            section: GitSection::Unstaged,
            renamed_from,
        });
    }
}

/// Parses `git status` into file groups, keeping the raw output beside it as
/// the poll fingerprint.
///
/// # Errors
///
/// Returns [`run_git`]'s message when the status call fails.
fn load_status(root: &Path) -> Result<(GitStatus, String), String> {
    let output = run_git(root, STATUS_ARGS)?;
    Ok((parse_status_porcelain(&output), output))
}

fn parse_unified_diff(path: &str, staged: bool, output: &str) -> LoadedDiff {
    let mut lines = Vec::new();
    let mut added = 0usize;
    let mut removed = 0usize;
    let (mut old_no, mut new_no) = (0usize, 0usize);
    let mut binary = false;
    let mut truncated = output.len() > MAX_DIFF_BYTES;
    let output = &output[..output.floor_char_boundary(output.len().min(MAX_DIFF_BYTES))];
    let mut in_hunk = false;
    for raw in output.lines() {
        if raw.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if raw.starts_with("Binary files ") {
            binary = true;
            break;
        }
        if lines.len() >= MAX_DIFF_LINES {
            truncated = true;
            break;
        }
        if let Some(hunk) = raw.strip_prefix("@@ ") {
            in_hunk = true;
            let mut numbers = String::new();
            for ch in hunk.chars() {
                if ch.is_ascii_digit() || ch == '-' || ch == '+' || ch == ',' || ch == ' ' {
                    numbers.push(ch);
                } else {
                    break;
                }
            }
            let mut parts = numbers.split_whitespace();
            let old_part = parts.next().unwrap_or_default().trim_start_matches('-');
            let new_part = parts.next().unwrap_or_default().trim_start_matches('+');
            let old_start = old_part
                .split(',')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            let new_start = new_part
                .split(',')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            old_no = old_start;
            new_no = new_start;
            lines.push(DiffLine {
                old_no: None,
                new_no: None,
                kind: DiffKind::Hunk,
                text: format!("@@ {numbers}@@"),
            });
            continue;
        }
        // Once a hunk begins, `--- text` and `+++ text` are content.
        // Only the next file boundary returns us to parsing headers.
        if !in_hunk || raw.starts_with('\\') {
            continue;
        }
        if let Some(text) = raw.strip_prefix('+') {
            new_no += 1;
            added += 1;
            lines.push(DiffLine {
                old_no: None,
                new_no: Some(new_no - 1),
                kind: DiffKind::Added,
                text: text.to_string(),
            });
        } else if let Some(text) = raw.strip_prefix('-') {
            old_no += 1;
            removed += 1;
            lines.push(DiffLine {
                old_no: Some(old_no - 1),
                new_no: None,
                kind: DiffKind::Removed,
                text: text.to_string(),
            });
        } else if let Some(text) = raw.strip_prefix(' ') {
            old_no += 1;
            new_no += 1;
            lines.push(DiffLine {
                old_no: Some(old_no - 1),
                new_no: Some(new_no - 1),
                kind: DiffKind::Context,
                text: text.to_string(),
            });
        }
    }
    LoadedDiff {
        path: path.to_string(),
        staged,
        untracked: false,
        commit: None,
        lines,
        added,
        removed,
        scroll: 0,
        binary,
        truncated,
    }
}

/// Worktree diff of one file for the left-pane viewer.
///
/// # Errors
///
/// Returns a message when the file cannot be read (untracked) or its `git
/// diff` call fails.
fn load_diff(root: &Path, file: &GitFile) -> Result<LoadedDiff, String> {
    if file.section == GitSection::Untracked {
        use std::io::Read;

        let abs = root.join(&file.path);
        let mut bytes = Vec::new();
        std::fs::File::open(&abs)
            .and_then(|input| {
                input
                    .take(MAX_DIFF_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map_err(|e| format!("could not read {}: {e}", file.path))?;
        if bytes.len() > MAX_DIFF_BYTES {
            return Ok(LoadedDiff {
                path: file.path.clone(),
                staged: false,
                untracked: true,
                commit: None,
                lines: Vec::new(),
                added: 0,
                removed: 0,
                scroll: 0,
                binary: false,
                truncated: true,
            });
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(LoadedDiff {
                path: file.path.clone(),
                staged: false,
                untracked: true,
                commit: None,
                lines: Vec::new(),
                added: 0,
                removed: 0,
                scroll: 0,
                binary: true,
                truncated: false,
            });
        };
        let mut lines = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if index >= MAX_DIFF_LINES {
                return Ok(LoadedDiff {
                    path: file.path.clone(),
                    staged: false,
                    untracked: true,
                    commit: None,
                    lines,
                    added: index,
                    removed: 0,
                    scroll: 0,
                    binary: false,
                    truncated: true,
                });
            }
            lines.push(DiffLine {
                old_no: None,
                new_no: Some(index + 1),
                kind: DiffKind::Added,
                text: line.to_string(),
            });
        }
        let added = lines.len();
        return Ok(LoadedDiff {
            path: file.path.clone(),
            staged: false,
            untracked: true,
            commit: None,
            lines,
            added,
            removed: 0,
            scroll: 0,
            binary: false,
            truncated: false,
        });
    }
    let staged = file.section == GitSection::Staged;
    // Keep full-file context when it fits. Otherwise ask Git for compact
    // hunks so unchanged lines cannot consume the entire display budget.
    let mut flags = vec!["--no-color", "--no-ext-diff", "-U10000"];
    if staged {
        flags.push("--cached");
    }
    let output = run_git_files(root, "diff", &flags, &[&file.path])?;
    let mut diff = parse_unified_diff(&file.path, staged, &output);
    if diff.truncated {
        flags[2] = "-U3";
        let output = run_git_files(root, "diff", &flags, &[&file.path])?;
        diff = parse_unified_diff(&file.path, staged, &output);
    }
    Ok(diff)
}

/// Sorted local branch names for the branch switcher.
///
/// # Errors
///
/// Returns [`run_git`]'s message when the branch listing fails.
fn load_branches(root: &Path) -> Result<Vec<String>, String> {
    let output = run_git(root, &["branch", "--format=%(refname:short)"])?;
    let mut branches: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    branches.sort();
    Ok(branches)
}

/// Parses `git log --format=%H%x1f%h%x1f%D%x1f%an%x1f%ad%x1f%s` output. The
/// subject is last so a stray separator inside a message cannot shift the
/// earlier fields.
fn parse_history(output: &str) -> Vec<HistoryEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\x1f');
            Some(HistoryEntry {
                hash: parts.next()?.to_string(),
                short: parts.next()?.to_string(),
                refs: parts.next().unwrap_or_default().to_string(),
                author: parts.next().unwrap_or_default().to_string(),
                date: parts.next().unwrap_or_default().to_string(),
                subject: parts.collect::<Vec<_>>().join("\x1f"),
            })
        })
        .filter(|entry| !entry.hash.is_empty())
        .collect()
}

fn load_history(root: &Path) -> Vec<HistoryEntry> {
    run_git(
        root,
        &[
            "log",
            "--format=%H%x1f%h%x1f%D%x1f%an%x1f%ad%x1f%s",
            "--date=short",
            "-30",
        ],
    )
    .map(|output| parse_history(&output))
    .unwrap_or_default()
}

/// Full-file diff of a historical commit for the left-pane viewer.
///
/// # Errors
///
/// Returns [`run_git`]'s message when `git show` fails (e.g. a pruned commit).
fn load_commit_diff(root: &Path, entry: &HistoryEntry) -> Result<LoadedDiff, String> {
    let mut args = [
        "show",
        "--no-color",
        "--no-ext-diff",
        "-U10000",
        "--format=",
        "--first-parent",
        entry.hash.as_str(),
        "--",
    ];
    let output = run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)?;
    let title = format!("{} {}", entry.short, truncate_visible(&entry.subject, 60));
    let mut diff = parse_unified_diff(&title, false, &output);
    if diff.truncated {
        args[3] = "-U3";
        let output = run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)?;
        diff = parse_unified_diff(&title, false, &output);
    }
    diff.commit = Some(entry.short.clone());
    diff.scroll = first_change_scroll(&diff.lines);
    Ok(diff)
}

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

/// One screen row of the file area: a group header or a file (flat index).
enum FileAreaRow {
    Header(GitSection),
    File(usize),
}

/// Ordered file-area rows (group headers + files) before scrolling.
fn file_area_rows(view: &GitView) -> Vec<FileAreaRow> {
    // One row per file plus at most one header per non-empty section.
    let mut rows = Vec::with_capacity(view.flat.len() + 3);
    let mut section = None;
    for (index, file) in view.flat.iter().enumerate() {
        if section != Some(file.section) {
            rows.push(FileAreaRow::Header(file.section));
            section = Some(file.section);
        }
        rows.push(FileAreaRow::File(index));
    }
    rows
}

/// First visible row, shared by rendering and mouse hit-testing.
fn file_area_start(view: &GitView, rows: &[FileAreaRow], file_capacity: usize) -> usize {
    view.file_scroll
        .min(rows.len().saturating_sub(file_capacity))
}

/// First visible history row for the current history scroll position.
fn history_top(view: &GitView, visible: usize) -> usize {
    if view.history.len() <= visible {
        0
    } else {
        view.history_scroll.min(view.history.len() - visible)
    }
}

/// Truncates plain text to a visible width without padding.
fn truncate_visible(text: &str, width: usize) -> String {
    super::markdown::split_chars(text, width.max(1))
        .into_iter()
        .next()
        .unwrap_or_default()
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

// ---------------------------------------------------------------------------
// Entry points used by the TUI shell.
// ---------------------------------------------------------------------------

/// Repository-init modal opened by `/git` outside a work tree. Collects the
/// remote URL, then runs the classic first-commit flow: `init`, a `README.md`
/// seed commit on `main`, `remote add origin`, and `push -u origin main`.
#[derive(Clone)]
pub(super) struct GitInitFlow {
    dir: PathBuf,
    remote: String,
    cursor: usize,
    error: Option<String>,
}

impl GitInitFlow {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            remote: String::new(),
            cursor: 0,
            error: None,
        }
    }
}

/// First-commit flow with the process environment (see [`init_and_push_env`]
/// for the retryable, documented version).
fn init_and_push(dir: &Path, remote: &str) -> Result<String, String> {
    init_and_push_env(dir, remote, &[])
}

/// First-commit flow with extra child `env` (see [`run_git_env`]): `git
/// init`, a seeded `README.md` commit on `main`, then — unless `remote` is
/// blank — `origin` setup plus `push -u origin main`. An existing
/// `README.md` is kept as is; only a missing one is seeded.
///
/// Every step is idempotent, so pressing Enter again after a push failure
/// resumes instead of failing: `init` and the branch rename re-run
/// harmlessly, the README seed keeps an existing file, the commit is
/// skipped when nothing is staged, and an existing `origin` gets its URL
/// updated instead of erroring on `remote add`.
///
/// # Errors
///
/// Returns a message naming the failed step (`init`, `add`, `commit`,
/// branch rename, remote setup). A failed push reports that the local
/// repository is still ready on `main`, so callers can keep going.
fn init_and_push_env(dir: &Path, remote: &str, env: &[(&str, &str)]) -> Result<String, String> {
    let git = |args: &[&str]| run_git_env(dir, args, env);
    git(&["init"]).map_err(|error| format!("git init failed: {error}"))?;
    let readme = dir.join("README.md");
    if !readme.exists() {
        let title = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Project");
        std::fs::write(&readme, format!("# {title}\n"))
            .map_err(|error| format!("could not write README.md: {error}"))?;
    }
    git(&["add", "README.md"]).map_err(|error| format!("git add failed: {error}"))?;
    // A retry after a partial run has nothing new staged; committing would
    // fail with "nothing to commit", so reuse the existing first commit.
    if git(&["diff", "--cached", "--quiet"]).is_err() {
        git(&["commit", "-m", "first commit"])
            .map_err(|error| format!("git commit failed: {error}"))?;
    }
    git(&["branch", "-M", "main"])
        .map_err(|error| format!("could not rename the branch to main: {error}"))?;
    let remote = remote.trim();
    if remote.is_empty() {
        return Ok("Initialized a local git repository on branch main.".to_string());
    }
    if git(&["remote", "get-url", "origin"]).is_ok() {
        git(&["remote", "set-url", "origin", remote])
            .map_err(|error| format!("git remote set-url failed: {error}"))?;
    } else {
        git(&["remote", "add", "origin", remote])
            .map_err(|error| format!("git remote add failed: {error}"))?;
    }
    match git(&["push", "-u", "origin", "main"]) {
        Ok(_) => Ok(format!(
            "Initialized a git repository and pushed to {remote}."
        )),
        Err(error) => Err(format!(
            "The local repository is ready on branch main, but the push failed: {error}"
        )),
    }
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

/// Single-line text editing shared by the commit box and the init-modal URL
/// field: insertion, deletion, and word/line motions over char indices.
fn line_insert(text: &mut String, cursor: &mut usize, insert: &str) {
    let mut chars: Vec<char> = text.chars().collect();
    let at = (*cursor).min(chars.len());
    let insert: Vec<char> = insert.chars().filter(|c| *c != '\n').collect();
    let count = insert.len();
    chars.splice(at..at, insert);
    *text = chars.into_iter().collect();
    *cursor = at + count;
}

fn line_edit(text: &mut String, cursor: &mut usize, key: Key) -> bool {
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

fn open_selected_diff(state: &mut ViewState) {
    jobs::start(state, "Loading diff…", operations::open_selected_diff);
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

/// Shows a historical commit's full diff in the left pane.
fn open_commit_diff(state: &mut ViewState) {
    jobs::start(state, "Loading commit…", operations::open_commit_diff);
}

/// Runs `tool` with `flags` over status-derived `paths` as literal
/// pathspecs (`:(literal)`), so names like `*.rs` never glob-match other
/// files, with `--` ending option parsing. Every command that takes a
/// filename from `git status` goes through here. (`--literal-pathspecs`
/// would read better but `git add` rejects it; the magic works everywhere
/// pathspecs do.)
fn run_git_files(
    root: &Path,
    tool: &str,
    flags: &[&str],
    paths: &[&str],
) -> Result<String, String> {
    let literal: Vec<String> = paths
        .iter()
        .map(|path| format!(":(literal){path}"))
        .collect();
    let mut args = Vec::with_capacity(flags.len() + literal.len() + 2);
    args.push(tool);
    args.extend_from_slice(flags);
    args.push("--");
    args.extend(literal.iter().map(String::as_str));
    if tool == "diff" {
        // The extra byte tells the parser the captured preview is partial.
        run_git_output(root, &args, &[], MAX_DIFF_BYTES + 1)
    } else {
        run_git(root, &args)
    }
}

/// Filenames a file operation targets. Staged renames (`R old -> new`) need
/// both sides: touching only `new` leaves `D old` behind in the index.
/// Unstaged and untracked entries target `new` alone — `old` is already
/// staged (or absent), and adding it would stage its deletion.
fn target_paths<'a>(
    section: GitSection,
    path: &'a str,
    renamed_from: Option<&'a str>,
) -> Vec<&'a str> {
    let mut paths = Vec::with_capacity(2);
    if section == GitSection::Staged
        && let Some(old) = renamed_from
    {
        paths.push(old);
    }
    paths.push(path);
    paths
}

fn set_file_staged(state: &mut ViewState, index: usize, staged: bool) {
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

/// Whether git's stderr reports an unknown subcommand (pre-2.23 git has no
/// `restore`). `run_git` forces `LC_ALL=C`, so the English marker is stable.
/// Always matched on the immediate command error, never on notice text — a
/// file path can contain anything.
fn is_unknown_subcommand(error: &str) -> bool {
    error.contains("is not a git command")
}

fn confirm_action(state: &mut ViewState) {
    if jobs::start(state, "Discarding…", operations::confirm_action)
        && let Some(view) = state.git_view.as_mut()
    {
        view.confirm = None;
    }
}

pub(super) fn handle_event(state: &mut ViewState, _editor: &mut Editor, event: Event) {
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
            if view.show_log {
                if amount > 0 {
                    view.log_scroll = view.log_scroll.saturating_add(amount as usize);
                } else {
                    view.log_scroll = view
                        .log_scroll
                        .saturating_sub(amount.unsigned_abs() as usize);
                }
            } else if view.focus == GitFocus::Diff || view.diff.is_some() {
                if let Some(diff) = view.diff.as_mut() {
                    if amount > 0 {
                        diff.scroll = diff.scroll.saturating_add(amount as usize);
                    } else {
                        diff.scroll = diff.scroll.saturating_sub(amount.unsigned_abs() as usize);
                    }
                }
            } else if view.focus == GitFocus::History {
                if amount > 0 {
                    view.history_scroll = view.history_scroll.saturating_add(amount as usize);
                } else {
                    view.history_scroll = view
                        .history_scroll
                        .saturating_sub(amount.unsigned_abs() as usize);
                }
            } else if amount > 0 {
                view.file_scroll = view.file_scroll.saturating_add(amount as usize);
            } else {
                view.file_scroll = view
                    .file_scroll
                    .saturating_sub(amount.unsigned_abs() as usize);
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

pub(super) fn clear_hover(state: &mut ViewState) -> bool {
    state
        .git_view
        .as_mut()
        .is_some_and(|view| view.mouse_position.take().is_some())
}

pub(super) fn pointer_over_control(state: &ViewState) -> bool {
    state.git_view.as_ref().is_some_and(|view| {
        !matches!(
            view.mouse_target(),
            None | Some(MouseTarget::CommitInput | MouseTarget::DismissMenu)
        )
    })
}

fn handle_mouse(state: &mut ViewState, mouse: MouseEvent) {
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
        }
        None => {}
    }
}

fn handle_key(state: &mut ViewState, key: Key) {
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
                    diff.scroll = diff.scroll.saturating_sub(1);
                }
            }
            Key::Down | Key::Char('j') => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    diff.scroll = diff.scroll.saturating_add(1);
                }
            }
            Key::PageUp => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
                    diff.scroll = diff.scroll.saturating_sub(10);
                }
            }
            Key::PageDown => {
                if let Some(view) = state.git_view.as_mut()
                    && let Some(diff) = view.diff.as_mut()
                {
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
                let root = state.git_view.as_ref().map(|view| view.root.clone());
                if let Some(root) = root {
                    let history = load_history(&root);
                    if let Some(view) = state.git_view.as_mut() {
                        view.show_log = !view.show_log;
                        view.history = history;
                        view.history_selected = 0;
                        view.log_scroll = 0;
                        view.diff = None;
                        view.show_branches = false;
                    }
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
pub(super) fn handle_interrupt(state: &mut ViewState) -> bool {
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

// ---------------------------------------------------------------------------
// Repository-init modal (outside a work tree).
// ---------------------------------------------------------------------------

/// Key handling for the init modal. `Enter` starts a background first-commit
/// job; failures keep the modal open with the error shown so the URL can be
/// fixed and retried, while typing clears a stale error.
pub(super) fn handle_init_event(state: &mut ViewState, event: Event) {
    if matches!(event, Event::Tick) {
        jobs::poll(state);
    }
    if matches!(event, Event::Key(Key::Ctrl('c'))) && jobs::cancel(state) {
        return;
    }

    let Some(flow) = state.git_init.as_mut() else {
        return;
    };
    match event {
        Event::Tick | Event::FocusGained | Event::FocusLost => {}
        Event::Paste(text) => {
            line_insert(&mut flow.remote, &mut flow.cursor, &text.replace('\n', " "));
            flow.error = None;
        }
        Event::MouseScroll(_) | Event::Mouse(_) => {}
        Event::Key(Key::Escape) => {
            jobs::cancel(state);
            state.git_init = None;
        }
        Event::Key(Key::Enter) => {
            let dir = flow.dir.clone();
            let remote = flow.remote.clone();
            jobs::start(
                state,
                "Initializing repository…",
                move |state| match init_and_push(&dir, remote.trim()) {
                    Ok(message) => {
                        state.git_init = None;
                        operations::open_dashboard(state);
                        state.notice(message);
                    }
                    Err(error) => {
                        if let Some(flow) = state.git_init.as_mut() {
                            flow.error = Some(error);
                        }
                    }
                },
            );
        }
        Event::Key(key) => {
            line_edit(&mut flow.remote, &mut flow.cursor, key);
            flow.error = None;
        }
    }
}

/// Full-screen init modal: a centered box with the remote-URL field. Returns
/// the frame plus the 1-based `CUP` cursor inside the URL field.
pub(super) fn render_init(
    state: &ViewState,
    _editor: &Editor,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    const DIM: &str = "\x1b[2m";
    const RESET: &str = "\x1b[0m";
    const RED_BOLD: &str = "\x1b[1;31m";
    let columns = columns.max(40);
    let rows = rows.max(10);
    let Some(flow) = state.git_init.as_ref() else {
        return (vec![" ".repeat(columns); rows], HIDDEN_CURSOR);
    };
    let width = columns.saturating_sub(4).clamp(30, 68);
    let left = columns.saturating_sub(width) / 2;
    let trailing = columns.saturating_sub(left + width);
    let inner = width.saturating_sub(2).max(10);
    let mut top = format!("{DIM}╭─ Initialize git repository ");
    top.push_str(&"─".repeat(inner.saturating_sub(markdown::visible_width(&top))));
    top.push_str(&format!("╮{RESET}"));
    let dir_line = format!(
        "│ {}",
        truncate_visible(
            &sanitize_plain(&format!("Not a git repository: {}", flow.dir.display())),
            inner.saturating_sub(2).max(8),
        )
    );
    let label = truncate_visible(
        "│ Remote URL (empty = local repository only):",
        inner.max(8),
    );
    let field_width = inner.saturating_sub(4).max(8);
    let shown = sanitize_plain(&commit_visible_text(&flow.remote, flow.cursor, field_width));
    let input = format!("│ > {shown}");
    let info = match &flow.error {
        Some(error) => format!(
            "{RED_BOLD}│ {}{RESET}",
            truncate_visible(&sanitize_plain(error), inner.saturating_sub(2).max(8))
        ),
        None => format!(
            "{DIM}{}{RESET}",
            truncate_visible(
                "│ Runs: init · seed README · commit · branch main · push -u origin main",
                inner.max(8),
            )
        ),
    };
    let mut bottom = format!("{DIM}╰─ Enter initialize · Esc cancel ");
    bottom.push_str(&"─".repeat(inner.saturating_sub(markdown::visible_width(&bottom))));
    bottom.push_str(&format!("╯{RESET}"));
    let box_lines = [top, dir_line, label, input, info, bottom];
    let box_top = (rows / 2).saturating_sub(3);
    let mut frame = vec![" ".repeat(columns); rows];
    for (offset, line) in box_lines.iter().enumerate() {
        let row = box_top + offset;
        if row >= rows - 1 {
            break;
        }
        frame[row] = format!(
            "{}{}{}",
            " ".repeat(left),
            markdown::fit_width(line, width),
            " ".repeat(trailing)
        );
    }
    frame[rows - 1] = markdown::fit_width(
        &format!(
            "{}{}\x1b[0m",
            status_style(accent_of(state)),
            " Enter initialize · Esc cancel (empty URL = local-only repository)"
        ),
        columns,
    );
    let len = flow.remote.chars().count();
    let shown_cursor = if len <= field_width {
        flow.cursor.min(len)
    } else {
        shown.chars().count()
    };
    let cursor = (
        box_top + 3 + 1,
        (left + 4 + shown_cursor + 1).min(columns.saturating_sub(1)),
    );
    (frame, cursor)
}

// ---------------------------------------------------------------------------
// Rendering: left transcript/diff/log/branches + right CHANGES card.
// ---------------------------------------------------------------------------

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

pub(super) fn render(
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

    // Fixed right-panel chrome rows: title, commit box, commit button, meta,
    // notice, blank.
    const FIXED_ROWS: usize = 6;
    // HISTORY section: header + entry rows + two detail rows. It gets a
    // bounded slice so the file list keeps the majority of the space.
    let history_capacity = content_height
        .saturating_sub(FIXED_ROWS + 4 + 3)
        .clamp(2, 5);
    let file_capacity = content_height
        .saturating_sub(FIXED_ROWS + 1 + history_capacity + 2)
        .max(1);
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
            right_width,
            commit_row: 1,
            commit_x0: divider + 1 + 2,
            commit_x1: columns,
            commit_button_row: 2,
            commit_button_x0: divider + 1,
            commit_button_x1: divider + 1 + commit_button_width(right_width),
            menu_button_x0: divider + 1 + commit_button_width(right_width),
            menu_button_x1: divider + 1 + right_width,
            close_button: None,
            file_rows: Vec::new(),
            actions: Vec::new(),
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
        // HISTORY rows follow the file area: header, entries, detail footer.
        let hist_top = history_top(view, history_capacity);
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

fn accent_of(state: &ViewState) -> UiColor {
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

fn commit_visible_text(commit: &str, cursor: usize, width: usize) -> String {
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
/// index plus one and the column accounts for the divider, the `"> "` prefix,
/// and the visible cursor offset within the (possibly scrolled) message.
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
        (divider + 4 + cursor_in_shown).min(columns.saturating_sub(1)),
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
    lines.push(markdown::fit_width(
        &format!("\x1b[1mHISTORY ({})\x1b[0m", view.history.len()),
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
    for (offset, entry) in view.history.iter().enumerate().skip(top).take(visible) {
        let is_selected = offset == view.history_selected && view.focus == GitFocus::History;
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
    // Commit message row.
    let commit_inner = if view.commit.is_empty() {
        "\x1b[2mMessage (e to edit, Enter to commit)…\x1b[0m".to_string()
    } else {
        sanitize_plain(&commit_visible(view, width.saturating_sub(4)))
    };
    let commit_line = if view.focus == GitFocus::Commit {
        format!(
            "{}> \x1b[0m{}",
            foreground_color(state.accent_color),
            markdown::fit_width(&commit_inner, width.saturating_sub(2))
        )
    } else {
        format!(
            "> {}",
            markdown::fit_width(&commit_inner, width.saturating_sub(2))
        )
    };
    lines.push(markdown::fit_width(&commit_line, width));
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
        "\x1b[2m{} · {} staged · {} unstaged · {} untracked\x1b[0m",
        sanitize_plain(view.status.upstream.as_deref().unwrap_or("no upstream")),
        view.status.staged.len(),
        view.status.unstaged.len(),
        view.status.untracked.len()
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
                        GitSection::Unstaged => view.status.unstaged.len(),
                        GitSection::Untracked => view.status.untracked.len(),
                    };
                    lines.push(markdown::fit_width(
                        &format!("\x1b[1m{} ({count})\x1b[0m", section.title()),
                        width,
                    ));
                }
                FileAreaRow::File(i) => {
                    let file = &view.flat[*i];
                    let selected = *i == view.selected && view.focus != GitFocus::Commit;
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
    let window = super::render::render_transcript_window(state, width, height, ImageSupport::None);
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

fn render_branches(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
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

fn render_log(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
    let Some(view) = state.git_view.as_ref() else {
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
        let top = view.log_scroll.min(max_top);
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

fn render_diff_pane(state: &mut ViewState, width: usize, height: usize) -> Vec<String> {
    let Some(view) = state.git_view.as_ref() else {
        return vec![String::new(); height];
    };
    let Some(diff) = view.diff.as_ref() else {
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
        let max_top = diff.lines.len().saturating_sub(body_height);
        let top = diff.scroll.min(max_top);
        // Gutter: 4-wide old number + space + 4-wide new number + space +
        // prefix + space = 12 columns.
        let code_width = width.saturating_sub(12).max(8);
        for line in diff.lines.iter().skip(top).take(body_height) {
            lines.push(render_diff_line(line, language, code_width, width));
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

fn render_diff_line(line: &DiffLine, language: &str, code_width: usize, width: usize) -> String {
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
        return markdown::fit_width(&format!("{HUNK}{}…{RESET}", line.text), width);
    }
    let old = line
        .old_no
        .map_or("    ".to_string(), |n| format!("{n:>4}"));
    let new = line
        .new_no
        .map_or("    ".to_string(), |n| format!("{n:>4}"));
    if line.kind == DiffKind::Context {
        let highlighted = if language.is_empty() {
            sanitize_diff_text(&line.text, code_width)
        } else {
            let rendered = super::highlight::render_line(language, &line.text);
            markdown::fit_width(&rendered, code_width)
        };
        return markdown::fit_width(&format!("{DIM}{old} {new}  {RESET} {highlighted}"), width);
    }
    let (bg, fg, prefix_style, prefix) = match line.kind {
        DiffKind::Added => (ADD_BG, ADD_FG, ADD_PREFIX, "+"),
        DiffKind::Removed => (DEL_BG, DEL_FG, DEL_PREFIX, "-"),
        // Hunk and Context lines return above; naming them keeps the match
        // exhaustive so a new `DiffKind` variant fails to compile here.
        DiffKind::Hunk | DiffKind::Context => (DEL_BG, DEL_FG, DEL_PREFIX, "-"),
    };
    let truncated = super::markdown::split_chars(&line.text, code_width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let highlighted = if language.is_empty() {
        sanitize_plain(&truncated)
    } else {
        super::highlight::render_line(language, &truncated)
    };
    // Keep the background alive across syntax-highlight resets.
    let rearm = format!("{RESET}{bg}{fg}");
    let body = highlighted.replace(RESET, &rearm);
    let inner = format!("{old} {new} {prefix_style}{prefix}{RESET}{bg}{fg} {body}");
    let visible = markdown::visible_width(&inner);
    if visible > width {
        return markdown::fit_width(&inner, width);
    }
    format!("{bg}{fg}{inner}{}\x1b[0m", " ".repeat(width - visible))
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

fn sanitize_diff_text(text: &str, width: usize) -> String {
    let clean: String = text
        .chars()
        .map(|c| {
            if c == '\t' {
                ' '
            } else if c.is_control() {
                '�'
            } else {
                c
            }
        })
        .collect();
    markdown::fit_width(&clean, width)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_header_parses_ahead_and_behind() {
        let mut status = GitStatus::default();
        parse_branch_header("## main...origin/main [ahead 1, behind 2]", &mut status);
        assert_eq!(status.branch, "main");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (1, 2));

        let mut clean = GitStatus::default();
        parse_branch_header("## main", &mut clean);
        assert_eq!(clean.branch, "main");
        assert_eq!((clean.ahead, clean.behind), (0, 0));
    }

    #[test]
    fn porcelain_splits_staged_unstaged_and_untracked() {
        let output = "## main...origin/main\x00M  staged.rs\x00 M unstaged.rs\x00MM both.rs\x00?? new.rs\x00";
        let status = parse_status_porcelain(output);
        assert_eq!(status.branch, "main");
        assert!(status.staged.iter().any(|f| f.path == "staged.rs"));
        assert!(status.unstaged.iter().any(|f| f.path == "unstaged.rs"));
        // MM appears on both sides.
        assert!(status.staged.iter().any(|f| f.path == "both.rs"));
        assert!(status.unstaged.iter().any(|f| f.path == "both.rs"));
        assert_eq!(status.untracked.len(), 1);
    }

    #[test]
    fn unified_diff_assigns_old_and_new_line_numbers() {
        let output = "@@ -1,3 +1,3 @@\n context\n-old\n+new\n context2\n";
        let diff = parse_unified_diff("a.rs", false, output);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
        let kinds: Vec<DiffKind> = diff.lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            vec![
                DiffKind::Hunk,
                DiffKind::Context,
                DiffKind::Removed,
                DiffKind::Added,
                DiffKind::Context
            ]
        );
        assert_eq!(diff.lines[1].old_no, Some(1));
        assert_eq!(diff.lines[1].new_no, Some(1));
        assert_eq!(diff.lines[2].old_no, Some(2));
        assert_eq!(diff.lines[2].new_no, None);
        assert_eq!(diff.lines[3].old_no, None);
        assert_eq!(diff.lines[3].new_no, Some(2));
    }

    #[test]
    fn diff_lines_fit_narrow_widths() {
        let line = DiffLine {
            old_no: Some(12),
            new_no: Some(13),
            kind: DiffKind::Added,
            text: "fn main() {}".to_string(),
        };
        let rendered = render_diff_line(&line, "rust", 8, 20);
        assert!(markdown::visible_width(&rendered) <= 20);
        assert!(markdown::strip_ansi(&rendered).contains('+'));
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
        let added_row = render_diff_line(&added, "rust", 30, 44);
        let removed_row = render_diff_line(&removed, "rust", 30, 44);
        let context_row = render_diff_line(&context, "rust", 30, 44);
        assert!(added_row.contains("48;2"));
        assert!(removed_row.contains("48;2"));
        assert!(!context_row.contains("48;2"));
        // Background spans the whole row, not just the code fragment.
        assert_eq!(markdown::visible_width(&added_row), 44);
        assert_eq!(markdown::visible_width(&removed_row), 44);
        // Syntax highlighting survives under the background tint.
        assert!(added_row.contains("1;34m") || added_row.contains("32m"));
    }

    #[test]
    fn commit_cursor_uses_one_based_terminal_coordinates() {
        // Divider at 64, commit row is frame index 1: the cursor belongs on
        // the second screen line, after "> " plus the typed text.
        assert_eq!(commit_cursor_position(64, 1, "rrw", 3, 30, 100), (2, 71));
        assert_eq!(commit_cursor_position(64, 1, "", 0, 30, 100), (2, 68));
        // Long messages scroll; the cursor stays on the visible tail.
        let long = "m".repeat(40);
        let (row, col) = commit_cursor_position(64, 1, &long, 40, 30, 100);
        assert_eq!(row, 2);
        assert!(col > 68 && col < 100);
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
    fn history_parses_hash_refs_author_and_subject() {
        let output = "abc1234567890\x1fabc1234\x1fHEAD -> main, origin/main\x1fAda\x1f2026-09-01\x1ffeat: add things\ndef5678\x1fdef5678\x1f\x1fBob\x1f2026-08-30\x1ffix it";
        let history = parse_history(output);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].short, "abc1234");
        assert_eq!(history[0].refs, "HEAD -> main, origin/main");
        assert_eq!(history[0].author, "Ada");
        assert_eq!(history[0].subject, "feat: add things");
        assert!(history[1].refs.is_empty());
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

    /// The poll fingerprint must move whenever the worktree does, and stay
    /// put otherwise — that is what makes external changes appear without
    /// reopening the panel while keeping idle polls redraw-free.
    #[test]
    fn status_raw_fingerprint_tracks_external_changes() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = std::env::temp_dir().join(format!(
            "yawl-git-poll-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        std::fs::write(work.join("note.txt"), "v1")?;
        git(&["add", "note.txt"])?;
        git(&[
            "-c",
            "user.name=yawl",
            "-c",
            "user.email=yawl@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init",
        ])?;

        let calm = git(STATUS_ARGS)?;
        // No change between reads: the poll must stay quiet.
        assert_eq!(git(STATUS_ARGS)?, calm);

        // An external edit (another terminal, an editor) moves it.
        std::fs::write(work.join("note.txt"), "v1\nexternal edit")?;
        let dirty = git(STATUS_ARGS)?;
        assert_ne!(dirty, calm);
        let status = parse_status_porcelain(&dirty);
        assert!(status.unstaged.iter().any(|file| file.path == "note.txt"));

        // So does staging it elsewhere.
        git(&["add", "note.txt"])?;
        let staged = git(STATUS_ARGS)?;
        assert_ne!(staged, dirty);
        let status = parse_status_porcelain(&staged);
        assert!(status.staged.iter().any(|file| file.path == "note.txt"));
        assert!(status.unstaged.is_empty());

        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    fn temp_dir_unique(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yawl-git-init-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Throwaway git identity as child-process env: `init_and_push_env`
    /// commits without touching (or needing) the developer's real git
    /// config. Passed per-`Command`, so — unlike process-environment
    /// mutation — it stays confined to spawned children under the
    /// multithreaded test harness. Returns the config path (kept alive by
    /// the temp dir) for the env array.
    fn write_test_gitconfig(dir: &Path) -> Result<String, crate::error::Error> {
        let config = dir.join("gitconfig");
        std::fs::write(
            &config,
            "[user]\n\tname = yawl tests\n\temail = yawl@localhost\n[commit]\n\tgpgsign = false\n",
        )?;
        config
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))
    }

    #[test]
    fn unknown_subcommand_detector_matches_git_stderr() {
        assert!(is_unknown_subcommand(
            "git: 'restore' is not a git command. See 'git --help'."
        ));
        assert!(!is_unknown_subcommand(
            "error: pathspec 'x' did not match any file(s) known to git"
        ));
        // Notice text mentioning restore (e.g. a file named
        // `my-restore-notes.txt`) must never trigger the old-git fallback.
        assert!(!is_unknown_subcommand("Discarded my-restore-notes.txt."));
    }

    #[test]
    fn find_root_names_plain_directories_as_non_repos() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let dir = temp_dir_unique("detect");
        std::fs::create_dir_all(&dir)?;
        let error = find_root(&dir).expect_err("a plain directory must not resolve as a repo");
        assert!(
            matches!(error, FindRootError::NotARepo),
            "a plain directory must report NotARepo, got {error:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[test]
    fn init_flow_pushes_first_commit_to_remote() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("push");
        let work = root.join("work");
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&bare)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let work_git = |args: &[&str]| {
            run_git(&work, args)
                .map(|output| output.trim().to_string())
                .map_err(crate::error::Error::Protocol)
        };
        run_git(&bare, &["init", "--bare"]).map_err(crate::error::Error::Protocol)?;
        let remote = bare
            .to_str()
            .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))?;
        let message =
            init_and_push_env(&work, remote, &identity).map_err(crate::error::Error::Protocol)?;
        assert!(
            message.contains(remote),
            "success names the remote: {message}"
        );
        assert!(work.join(".git").is_dir());
        assert_eq!(std::fs::read_to_string(work.join("README.md"))?, "# work\n");
        assert_eq!(work_git(&["branch", "--show-current"])?, "main");
        assert_eq!(work_git(&["log", "-1", "--format=%s"])?, "first commit");
        assert_eq!(work_git(&["remote", "get-url", "origin"])?, remote);
        let head = work_git(&["rev-parse", "HEAD"])?;
        let remote_head = run_git(&bare, &["rev-parse", "HEAD"])
            .map(|output| output.trim().to_string())
            .map_err(crate::error::Error::Protocol)?;
        assert_eq!(head, remote_head);
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn init_flow_retry_resumes_after_a_push_failure() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("retry");
        let work = root.join("work");
        let bare = root.join("remote.git");
        std::fs::create_dir_all(&work)?;
        std::fs::create_dir_all(&bare)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let work_git = |args: &[&str]| {
            run_git(&work, args)
                .map(|output| output.trim().to_string())
                .map_err(crate::error::Error::Protocol)
        };
        let missing = root.join("no-such-remote.git");
        let missing = missing
            .to_str()
            .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))?;
        // First attempt builds the repo and commit, then fails the push.
        let error = init_and_push_env(&work, missing, &identity)
            .expect_err("pushing to a missing path must fail");
        assert!(error.contains("push failed"), "push failure: {error}");
        // Retrying the identical flow must not fail at `commit` ("nothing
        // to commit") or at `remote add` ("origin already exists").
        run_git(&bare, &["init", "--bare"]).map_err(crate::error::Error::Protocol)?;
        let remote = bare
            .to_str()
            .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))?;
        let message =
            init_and_push_env(&work, remote, &identity).map_err(crate::error::Error::Protocol)?;
        assert!(
            message.contains(remote),
            "retry pushes to the fixed remote: {message}"
        );
        assert_eq!(work_git(&["remote", "get-url", "origin"])?, remote);
        assert_eq!(work_git(&["log", "-1", "--format=%s"])?, "first commit");
        // Still exactly one commit: the retry reused it instead of amending
        // or erroring.
        assert_eq!(work_git(&["rev-list", "--count", "HEAD"])?, "1");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn init_flow_without_remote_stays_local() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("local");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let work_git = |args: &[&str]| {
            run_git(&work, args)
                .map(|output| output.trim().to_string())
                .map_err(crate::error::Error::Protocol)
        };
        let message =
            init_and_push_env(&work, "   ", &identity).map_err(crate::error::Error::Protocol)?;
        assert!(
            message.contains("local"),
            "blank URL skips the push: {message}"
        );
        assert_eq!(work_git(&["branch", "--show-current"])?, "main");
        assert_eq!(work_git(&["remote"])?, "");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn init_flow_keeps_an_existing_readme() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("readme");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("README.md"), "# Keep me\n")?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        init_and_push_env(&work, "", &identity).map_err(crate::error::Error::Protocol)?;
        assert_eq!(
            std::fs::read_to_string(work.join("README.md"))?,
            "# Keep me\n"
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn init_flow_reports_push_failure_but_keeps_the_local_repo() -> Result<(), crate::error::Error>
    {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("push-fail");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let missing = root.join("no-such-remote.git");
        let remote = missing
            .to_str()
            .ok_or_else(|| crate::error::Error::Protocol("non-UTF8 temp path".to_string()))?;
        let error = init_and_push_env(&work, remote, &identity)
            .expect_err("pushing to a missing path must fail");
        assert!(
            error.contains("push failed") && error.contains("ready on branch main"),
            "error keeps the local repo usable: {error}"
        );
        let branch = run_git(&work, &["branch", "--show-current"])
            .map(|output| output.trim().to_string())
            .map_err(crate::error::Error::Protocol)?;
        assert_eq!(branch, "main");
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn target_paths_carry_both_sides_of_staged_renames_only() {
        assert_eq!(
            target_paths(GitSection::Staged, "b.txt", Some("a.txt")),
            ["a.txt", "b.txt"]
        );
        assert_eq!(target_paths(GitSection::Staged, "b.txt", None), ["b.txt"]);
        // Unstaged entries target `new` alone: `old` is already staged, and
        // adding it would stage its deletion.
        assert_eq!(
            target_paths(GitSection::Unstaged, "b.txt", Some("a.txt")),
            ["b.txt"]
        );
        assert_eq!(
            target_paths(GitSection::Untracked, "b.txt", None),
            ["b.txt"]
        );
    }

    #[test]
    fn stage_targets_literal_filenames() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("literal");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        std::fs::write(work.join("*.rs"), "star")?;
        std::fs::write(work.join("other.rs"), "other")?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        // Without the literal pathspec, `*.rs` would glob-match `other.rs`.
        run_git_files(&work, "add", &[], &["*.rs"]).map_err(crate::error::Error::Protocol)?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        assert_eq!(
            status
                .staged
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["*.rs"]
        );
        assert!(status.untracked.iter().any(|file| file.path == "other.rs"));
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    #[test]
    fn unstaging_a_staged_rename_clears_both_sides() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        let root = temp_dir_unique("rename");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        let identity_path = write_test_gitconfig(&root)?;
        let identity = [("GIT_CONFIG_GLOBAL", identity_path.as_str())];
        let git = |args: &[&str]| {
            run_git_env(&work, args, &identity).map_err(crate::error::Error::Protocol)
        };
        git(&["init", "--template="])?;
        std::fs::write(work.join("a.txt"), "v1")?;
        git(&["add", "a.txt"])?;
        git(&["commit", "-m", "init"])?;
        git(&["mv", "a.txt", "b.txt"])?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        let renamed = status
            .staged
            .iter()
            .find(|file| file.path == "b.txt")
            .expect("rename should be staged");
        assert_eq!(renamed.renamed_from.as_deref(), Some("a.txt"));
        // Mirror `set_file_staged(…, staged = false)`: both sides restore.
        let paths = target_paths(
            GitSection::Staged,
            &renamed.path,
            renamed.renamed_from.as_deref(),
        );
        run_git_files(&work, "restore", &["--staged"], &paths)
            .map_err(crate::error::Error::Protocol)?;
        let status = parse_status_porcelain(&git(STATUS_ARGS)?);
        assert!(
            status.staged.is_empty(),
            "no staged side may survive, got {:?}",
            status.staged
        );
        assert!(
            status.unstaged.iter().any(|file| file.path == "a.txt"),
            "old path returns as an unstaged deletion"
        );
        assert!(
            status.untracked.iter().any(|file| file.path == "b.txt"),
            "new path returns as untracked"
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }

    /// Regression test for the `/git` diff hang: `run_git_env` used to poll
    /// `try_wait()` without draining stdout, so any child emitting more than
    /// the pipe buffer (~64 KiB, e.g. `diff -U10000` of `git.rs`) blocked on
    /// write() while we blocked on wait — forever. Reader threads must keep
    /// the pipes drained.
    #[test]
    fn large_cached_diff_does_not_deadlock_on_full_pipes() -> Result<(), crate::error::Error> {
        if !git_available() {
            return Ok(());
        }
        crate::set_interrupted(false);
        let root = temp_dir_unique("large-diff");
        let work = root.join("work");
        std::fs::create_dir_all(&work)?;
        // ~240 KiB staged file: well over the pipe buffer even as a diff.
        let mut big = String::with_capacity(256 * 1024);
        for index in 0..3000 {
            big.push_str(&format!(
                "line {index:05} padding padding padding padding padding padding padding\n"
            ));
        }
        assert!(big.len() > 64 * 1024);
        std::fs::write(work.join("big.rs"), &big)?;
        let git = |args: &[&str]| run_git(&work, args).map_err(crate::error::Error::Protocol);
        git(&["init", "--template="])?;
        git(&["add", "big.rs"])?;
        // Same full-file flags `load_diff` uses. Run off-thread with a
        // timeout so a reintroduced deadlock fails instead of hanging CI.
        let work_clone = work.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let output = run_git(
                &work_clone,
                &["diff", "--cached", "--no-color", "--no-ext-diff", "-U10000"],
            );
            let _ = done_tx.send(output);
        });
        let output = done_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("large git diff must finish; pipe deadlock?")
            .map_err(crate::error::Error::Protocol)?;
        assert!(
            output.len() > 64 * 1024,
            "diff should exceed the pipe buffer, got {} bytes",
            output.len()
        );
        let _ = std::fs::remove_dir_all(&root);
        Ok(())
    }
}

#[cfg(test)]
#[path = "git_tests.rs"]
mod regression_tests;
