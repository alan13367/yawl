//! Blocking Git work. Owned inputs and results cross the UI worker channel.
use super::*;

pub(super) struct OperationState {
    pub(super) git_view: Option<GitView>,
    pub(super) git_init: Option<GitInitFlow>,
    pub(super) notice: Option<String>,
}

impl OperationState {
    pub(super) fn notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
    }
}

pub(super) fn open_dashboard(state: &mut OperationState) {
    if !git_available() {
        state.notice("git is not installed.");
        return;
    }
    let cwd = crate::config::working_dir();
    let root = match find_root(&cwd) {
        Ok(root) => root,
        // Outside a work tree `/git` offers setup: a modal asking for the
        // remote, then the classic README-seed first push.
        Err(FindRootError::NotARepo) => {
            state.git_init = Some(GitInitFlow::new(cwd));
            return;
        }
        Err(error) => {
            state.notice(format!("Could not open git dashboard: {error}."));
            return;
        }
    };
    match load_status(&root) {
        Ok((status, raw)) => {
            let mut view = GitView::new(root, status);
            let (history, has_more) = load_history_limit(&view.root, view.history_limit);
            view.history = history;
            view.history_has_more = has_more;
            view.last_status_raw = Some(raw);
            view.last_poll = Some(Instant::now());
            state.git_view = Some(view);
        }
        Err(error) => state.notice(format!("Could not read git status: {error}")),
    }
}

pub(super) fn refresh(state: &mut OperationState) {
    let Some(view) = state.git_view.as_mut() else {
        return;
    };
    match load_status(&view.root) {
        Ok((status, raw)) => apply_status(view, status, raw),
        Err(error) => view.set_notice(format!("Refresh failed: {error}"), true),
    }
}

fn apply_status(view: &mut GitView, status: GitStatus, raw: String) {
    let selected_path = view.selected_file().map(|file| file.path.clone());
    let history_hash = view
        .history
        .get(view.history_selected)
        .map(|entry| entry.hash.clone());
    let commit = view.commit.clone();
    let commit_cursor = view.commit_cursor;
    view.last_status_raw = Some(raw);
    view.last_poll = Some(Instant::now());
    view.status = status;
    view.flat = view.status.flat();
    if view.flat.is_empty() {
        view.selected = 0;
    } else if let Some(path) = selected_path {
        view.selected = view
            .flat
            .iter()
            .position(|file| file.path == path)
            .unwrap_or(0);
    } else {
        view.selected = view.selected.min(view.flat.len() - 1);
    }
    view.commit = commit;
    view.commit_cursor = commit_cursor.min(view.commit.chars().count());
    let (history, has_more) = load_history_limit(&view.root, view.history_limit);
    view.history = history;
    view.history_has_more = has_more;
    if view.history.is_empty() {
        view.history_selected = 0;
    } else if let Some(hash) = history_hash {
        view.history_selected = view
            .history
            .iter()
            .position(|entry| entry.hash == hash)
            .unwrap_or(0);
    } else {
        view.history_selected = view.history_selected.min(view.history.len() - 1);
    }
    if let Err(error) = refresh_visible_diff(view) {
        view.set_notice(error, true);
    }
}

pub(super) fn do_commit_with_mode(state: &mut OperationState, mode: CommitMode) {
    let (root, message, amend) = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        (
            view.root.clone(),
            view.commit.trim().to_string(),
            view.amend,
        )
    };
    // Amending with an empty box reuses the previous message.
    let amend_no_edit = amend && message.is_empty();
    if message.is_empty() && !amend_no_edit {
        if let Some(view) = state.git_view.as_mut() {
            view.set_notice("Write a commit message first.".to_string(), true);
        }
        return;
    }
    // Sync pulls first so the commit lands on the fresh upstream instead of
    // diverging from it. A failed pull aborts before anything is committed
    // and leaves the message box intact for retry.
    if mode == CommitMode::AndSync
        && let Err(error) = run_git(&root, &["pull", "--ff-only"])
    {
        if let Some(view) = state.git_view.as_mut() {
            view.set_notice(format!("Sync failed before commit: {error}"), true);
        }
        refresh(state);
        return;
    }
    let mut args = vec!["commit"];
    if amend_no_edit {
        args.extend(["--amend", "--no-edit"]);
    } else {
        if amend {
            args.push("--amend");
        }
        args.extend(["-m", message.as_str()]);
    }
    if let Err(error) = run_git(&root, &args) {
        if let Some(view) = state.git_view.as_mut() {
            view.set_notice(error, true);
        }
        return;
    }
    let (notice, error) = if matches!(mode, CommitMode::AndPush | CommitMode::AndSync) {
        match run_git(&root, &["push"]) {
            Ok(_) => (
                if mode == CommitMode::AndSync {
                    "Committed and synced."
                } else {
                    "Committed and pushed."
                }
                .to_string(),
                false,
            ),
            Err(error) => (format!("Committed. Push failed: {error}"), true),
        }
    } else if amend {
        ("Amended commit.".to_string(), false)
    } else {
        ("Committed.".to_string(), false)
    };
    if let Some(view) = state.git_view.as_mut() {
        view.commit.clear();
        view.commit_cursor = 0;
        view.amend = false;
        view.commit_menu = None;
        view.set_notice(notice, error);
    }
    refresh(state);
}

pub(super) fn open_selected_diff(state: &mut OperationState) {
    let (root, file) = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        match view.selected_file().cloned() {
            Some(file) => (view.root.clone(), file),
            None => return,
        }
    };
    match load_diff(&root, &file) {
        Ok(mut diff) => {
            // Land on the first change with a couple of context lines above
            // it instead of the top of the file. The anchor resolves to
            // visual rows on the next render, when the pane width is known.
            diff.scroll = SCROLL_ANCHOR_PENDING;
            if let Some(view) = state.git_view.as_mut() {
                view.diff = Some(diff);
                view.show_log = false;
                view.show_branches = false;
                view.focus = GitFocus::Diff;
                view.set_notice(String::new(), false);
            }
        }
        Err(error) => {
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(error, true);
            }
        }
    }
}

pub(super) fn open_commit_diff(state: &mut OperationState) {
    let (root, entry) = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        match view.history.get(view.history_selected).cloned() {
            Some(entry) => (view.root.clone(), entry),
            None => return,
        }
    };
    match load_commit_diff(&root, &entry) {
        Ok(diff) => {
            if let Some(view) = state.git_view.as_mut() {
                view.diff = Some(diff);
                view.show_log = false;
                view.show_branches = false;
                view.focus = GitFocus::Diff;
                view.set_notice(String::new(), false);
            }
        }
        Err(error) => {
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(error, true);
            }
        }
    }
}

pub(super) fn set_file_staged(state: &mut OperationState, index: usize, staged: bool) {
    let (root, file) = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        match view.flat.get(index).cloned() {
            Some(file) => (view.root.clone(), file),
            None => return,
        }
    };
    let paths = target_paths(file.section, &file.path, file.renamed_from.as_deref());
    let result = if staged {
        run_git_files(&root, "add", &[], &paths)
    } else {
        // An implicit HEAD lets reset use the empty tree on an unborn
        // branch. Path-limited reset leaves working files untouched.
        run_git_files(&root, "reset", &[], &paths)
    };
    match result {
        Ok(_) => {
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(String::new(), false);
            }
            refresh(state);
        }
        Err(error) => {
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(error, true);
            }
        }
    }
}

pub(super) fn run_simple(state: &mut OperationState, args: &[&str], ok_message: &str) {
    let root = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        view.root.clone()
    };
    report_simple_result(state, run_git(&root, args), ok_message);
}

/// Run literal status-derived paths without glob expansion.
pub(super) fn run_simple_files(
    state: &mut OperationState,
    tool: &str,
    flags: &[&str],
    paths: &[&str],
    ok_message: &str,
) {
    let root = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        view.root.clone()
    };
    report_simple_result(state, run_git_files(&root, tool, flags, paths), ok_message);
}

/// Apply a completed command result before refreshing repository data.
pub(super) fn report_simple_result(
    state: &mut OperationState,
    result: Result<String, String>,
    ok_message: &str,
) {
    match result {
        Ok(output) => {
            let detail = output.trim();
            let message = if detail.is_empty() {
                ok_message.to_string()
            } else {
                format!("{ok_message} {detail}")
            };
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(message.trim().to_string(), false);
            }
            refresh(state);
        }
        Err(error) => {
            if let Some(view) = state.git_view.as_mut() {
                view.set_notice(error, true);
            }
        }
    }
}

pub(super) fn confirm_action(state: &mut OperationState) {
    let confirm = {
        let Some(view) = state.git_view.as_ref() else {
            return;
        };
        view.confirm.clone()
    };
    let Some(confirm) = confirm else { return };
    match confirm {
        Confirm::DiscardFile {
            path,
            renamed_from,
            untracked,
            staged,
        } => {
            let root = {
                let Some(view) = state.git_view.as_ref() else {
                    return;
                };
                view.root.clone()
            };
            // Staged renames resolve both sides; unstaged entries and plain
            // files target `new` alone (see `target_paths`).
            let section = if staged {
                GitSection::Staged
            } else if untracked {
                GitSection::Untracked
            } else {
                GitSection::Unstaged
            };
            let targets = target_paths(section, &path, renamed_from.as_deref());
            if untracked {
                // Respect ignore rules and nested repositories, including
                // when status has collapsed a directory into one row.
                if let Some(view) = state.git_view.as_mut() {
                    view.confirm = None;
                }
                run_simple_files(
                    state,
                    "clean",
                    &["-f", "-d"],
                    &targets,
                    "Cleaned untracked files.",
                );
            } else if staged {
                // Undo everything for a staged path: unstage and restore the
                // worktree to HEAD in one step.
                if let Some(view) = state.git_view.as_mut() {
                    view.confirm = None;
                }
                let discarded = format!("Discarded {path}.");
                match run_git_files(
                    &root,
                    "restore",
                    &["--source=HEAD", "--staged", "--worktree"],
                    &targets,
                ) {
                    Ok(_) => {
                        if let Some(view) = state.git_view.as_mut() {
                            view.set_notice(discarded, false);
                        }
                        refresh(state);
                    }
                    // Older git without `restore`: reset the index, then restore.
                    Err(error) if is_unknown_subcommand(&error) => {
                        run_simple_files(state, "reset", &["HEAD"], &targets, "");
                        run_simple_files(state, "checkout", &[], &targets, &discarded);
                    }
                    Err(error) => {
                        if let Some(view) = state.git_view.as_mut() {
                            view.set_notice(error, true);
                        }
                    }
                }
            } else {
                if let Some(view) = state.git_view.as_mut() {
                    view.confirm = None;
                }
                let discarded = format!("Discarded {path}.");
                match run_git_files(&root, "restore", &[], &targets) {
                    Ok(_) => {
                        if let Some(view) = state.git_view.as_mut() {
                            view.set_notice(discarded, false);
                        }
                        refresh(state);
                    }
                    // Older git without `restore`: fall back to checkout.
                    Err(error) if is_unknown_subcommand(&error) => {
                        run_simple_files(state, "checkout", &[], &targets, &discarded);
                    }
                    Err(error) => {
                        if let Some(view) = state.git_view.as_mut() {
                            view.set_notice(error, true);
                        }
                    }
                }
            }
        }
        Confirm::DiscardAll => {
            if let Some(view) = state.git_view.as_mut() {
                view.confirm = None;
            }
            run_simple(
                state,
                &["restore", "--", "."],
                "Discarded all unstaged changes.",
            );
        }
    }
}

/// Pages in the next batch of older commits. Appends to the same newest-first
/// order, so existing indices stay valid unless newer commits arrived
/// concurrently (remapped by hash like a refresh). Growing the limit instead
/// of offsetting keeps the load to one bounded `git log` per page.
pub(super) fn load_more_history(state: &mut OperationState) {
    let Some(view) = state.git_view.as_mut() else {
        return;
    };
    if !view.history_has_more {
        return;
    }
    let selected_hash = view
        .history
        .get(view.history_selected)
        .map(|entry| entry.hash.clone());
    let next_limit = view
        .history_limit
        .saturating_add(HISTORY_PAGE_SIZE)
        .max(view.history.len().saturating_add(1));
    let (history, has_more) = load_history_limit(&view.root, next_limit);
    // A short page proves the end of history even when the previous load hit
    // its limit exactly.
    if history.len() <= view.history.len() {
        view.history_has_more = false;
        return;
    }
    view.history_limit = next_limit;
    view.history = history;
    view.history_has_more = has_more;
    if view.history.is_empty() {
        view.history_selected = 0;
        view.history_scroll = 0;
    } else if let Some(hash) = selected_hash {
        view.history_selected = view
            .history
            .iter()
            .position(|entry| entry.hash == hash)
            .unwrap_or(0);
    } else {
        view.history_selected = view.history_selected.min(view.history.len() - 1);
    }
    view.history_scroll = view
        .history_scroll
        .min(view.history.len().saturating_sub(1));
}

/// Status is fetched once; only changed status needs a history refresh.
pub(super) fn poll(state: &mut OperationState) {
    let Some(view) = state.git_view.as_mut() else {
        return;
    };
    let Ok((status, raw)) = load_status(&view.root) else {
        return;
    };
    if view.last_status_raw.as_deref() == Some(raw.as_str()) {
        let _ = refresh_visible_diff(view);
        return;
    }
    apply_status(view, status, raw);
}
