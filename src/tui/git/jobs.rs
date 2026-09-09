//! Background Git jobs with UI-owned state and cancellation.
use std::sync::mpsc::{self, Receiver, TryRecvError};

use super::operations::OperationState;
use super::*;
use crate::cancellation::CancellationToken;

pub(in crate::tui) struct GitJob {
    receiver: Receiver<OperationState>,
    cancellation: CancellationToken,
    original: Option<GitView>,
    pub(super) label: &'static str,
}

impl Drop for GitJob {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

pub(super) fn start(
    state: &mut ViewState,
    label: &'static str,
    run: impl FnOnce(&mut OperationState) + Send + 'static,
) -> bool {
    if state
        .git_job
        .as_ref()
        .is_some_and(|job| job.label.is_empty())
        && !label.is_empty()
    {
        // Explicit actions supersede a read-only refresh. Dropping its token
        // cancels the obsolete subprocess; its result has no receiver now.
        state.git_job = None;
    }
    if state.git_job.is_some() {
        return false;
    }
    let original = state.git_view.clone();
    let mut input = OperationState {
        git_view: original.clone(),
        git_init: state.git_init.clone(),
        notice: None,
    };
    let cancellation = CancellationToken::default();
    let token = cancellation.clone();
    let (tx, receiver) = mpsc::channel();
    match std::thread::Builder::new()
        .name("yawl-git".into())
        .spawn(move || {
            crate::cancellation::scope(&token, || run(&mut input));
            let _ = tx.send(input);
        }) {
        Ok(_) => {
            state.git_job = Some(GitJob {
                receiver,
                cancellation,
                original,
                label,
            });
            true
        }
        Err(error) => {
            state.notice(format!("Could not start Git worker: {error}"));
            false
        }
    }
}

pub(super) fn cancel(state: &mut ViewState) -> bool {
    let Some(job) = &state.git_job else {
        return false;
    };
    job.cancellation.cancel();
    true
}

pub(super) fn poll(state: &mut ViewState) -> bool {
    let Some(job) = state.git_job.as_ref() else {
        return false;
    };
    let result = match job.receiver.try_recv() {
        Ok(result) => Some(result),
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => None,
    };
    let Some(job) = state.git_job.take() else {
        return false;
    };
    if job.cancellation.is_canceled() {
        if let Some(view) = state.git_view.as_mut() {
            view.last_poll = None;
            view.set_notice(
                "Git operation canceled; refreshing repository state.".into(),
                false,
            );
        }
        return true;
    }
    let Some(mut result) = result else {
        state.notice("Git worker stopped unexpectedly.");
        return true;
    };
    if let Some(original) = job.original.as_ref() {
        // Closing a dashboard while a job runs must never reopen it.
        if let (Some(current), Some(updated)) = (state.git_view.as_ref(), result.git_view.as_mut())
        {
            merge_ui(original, current, updated);
        } else {
            return true;
        }
    }
    if job.label.is_empty()
        && let (Some(current), Some(updated)) = (&mut state.git_view, &result.git_view)
        && current.status == updated.status
        && current.history == updated.history
        && current.diff == updated.diff
        && current.notice == updated.notice
    {
        current.last_poll = Some(Instant::now());
        return false;
    }
    // The init URL may be edited while initialization or push is running.
    if let (Some(current), Some(updated)) = (&state.git_init, &mut result.git_init) {
        updated.remote.clone_from(&current.remote);
        updated.cursor = current.cursor;
    }
    state.git_view = result.git_view;
    state.git_init = result.git_init;
    if let Some(notice) = result.notice {
        state.notice(notice);
    }
    true
}

/// Apply repository results while preserving edits and navigation made after
/// dispatch. A completed command may clear the original commit text, but never
/// text the user has typed in the meantime.
fn merge_ui(original: &GitView, current: &GitView, updated: &mut GitView) {
    macro_rules! preserve {
        ($($field:ident),* $(,)?) => { $(
            if current.$field != original.$field { updated.$field = current.$field.clone(); }
        )* };
    }
    preserve!(
        commit,
        commit_cursor,
        amend,
        focus,
        file_scroll,
        history_scroll,
        show_log,
        log_scroll,
        show_branches,
        branch_selected,
        confirm,
        commit_menu,
        mouse_position,
        notice,
        notice_error
    );
    if current.commit != original.commit {
        updated.commit_cursor = current.commit_cursor;
    }
    updated.layout = current.layout.clone();
    if current.selected != original.selected {
        updated.diff.clone_from(&current.diff);
        updated.focus = current.focus;
        updated.selected = current
            .selected_file()
            .and_then(|file| {
                updated.flat.iter().position(|candidate| {
                    candidate.path == file.path && candidate.section == file.section
                })
            })
            .unwrap_or(0);
    }
    if current.history_selected != original.history_selected {
        updated.history_selected = current
            .history
            .get(current.history_selected)
            .and_then(|entry| {
                updated
                    .history
                    .iter()
                    .position(|candidate| candidate.hash == entry.hash)
            })
            .unwrap_or(0);
    }
    match (&original.diff, &current.diff, &mut updated.diff) {
        (Some(old), Some(now), Some(new))
            if old.path == now.path && old.commit == now.commit && old.staged == now.staged =>
        {
            if new.path == now.path
                && new.commit == now.commit
                && new.staged == now.staged
                && now.scroll != old.scroll
            {
                new.scroll = now.scroll;
            }
        }
        _ if current.diff != original.diff => updated.diff.clone_from(&current.diff),
        _ => {}
    }
}

#[cfg(test)]
pub(in crate::tui) fn settle(state: &mut ViewState) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.git_job.is_some() {
        assert!(Instant::now() < deadline, "Git worker failed to settle");
        poll(state);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrolling_during_a_load_preserves_the_requested_diff() {
        let mut original = GitView::new(PathBuf::from("/unused"), GitStatus::default());
        let mut diff = parse_unified_diff("file.rs", false, "@@ -1 +1 @@\n-old\n+new\n");
        diff.commit = Some("first".into());
        original.diff = Some(diff);
        let mut current = original.clone();
        current.diff.as_mut().unwrap().scroll = 10;

        // Both a new commit and another file must survive scrolling the old preview.
        for commit in [Some("second".to_string()), None] {
            let mut updated = original.clone();
            let loaded = updated.diff.as_mut().unwrap();
            loaded.commit = commit;
            loaded.scroll = SCROLL_ANCHOR_PENDING;
            let expected = updated.diff.clone();
            merge_ui(&original, &current, &mut updated);
            assert_eq!(updated.diff, expected);
        }

        // A refresh of the same diff still keeps the user's scroll position.
        let mut updated = original.clone();
        merge_ui(&original, &current, &mut updated);
        assert_eq!(updated.diff.as_ref().unwrap().scroll, 10);

        // Closing the preview while the worker runs still takes precedence.
        current.diff = None;
        merge_ui(&original, &current, &mut updated);
        assert!(updated.diff.is_none());
    }
}
