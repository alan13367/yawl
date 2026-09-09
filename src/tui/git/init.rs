//! Repository initialization, retryable first push, and its modal.

use super::input::{line_edit, line_insert};
use super::render::{accent_of, commit_visible_text};
use super::repository::run_git_env;
use super::{jobs, operations};
use super::{sanitize_plain, truncate_visible};
use crate::tui::events::{Event, Key};
use crate::tui::input::Editor;
use crate::tui::render::{HIDDEN_CURSOR, status_style};
use crate::tui::{ViewState, markdown};
use std::path::{Path, PathBuf};

/// Repository-init modal opened by `/git` outside a work tree. Collects the
/// remote URL, then runs the classic first-commit flow: `init`, a `README.md`
/// seed commit on `main`, `remote add origin`, and `push -u origin main`.
#[derive(Clone)]
pub(in crate::tui) struct GitInitFlow {
    pub(super) dir: PathBuf,
    pub(super) remote: String,
    pub(super) cursor: usize,
    pub(super) error: Option<String>,
}

impl GitInitFlow {
    pub(in crate::tui) fn new(dir: PathBuf) -> Self {
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

/// Key handling for the init modal. `Enter` starts a background first-commit
/// job; failures keep the modal open with the error shown so the URL can be
/// fixed and retried, while typing clears a stale error.
pub(in crate::tui) fn handle_init_event(state: &mut ViewState, event: Event) {
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
pub(in crate::tui) fn render_init(
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

#[cfg(test)]
mod tests {
    use super::super::repository::{git_available, run_git};
    use super::super::test_support::{temp_dir_unique, write_test_gitconfig};
    use super::*;

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
        // The bare remote keeps its default HEAD (often master) even after
        // pushing main, so resolve the pushed branch explicitly.
        let remote_head = run_git(&bare, &["rev-parse", "refs/heads/main"])
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
}
