//! `/diff`: cards for files the file tools changed this session.

use crate::agent::Agent;

use super::super::state::ViewState;

/// Per-side content cap for `/diff` cards, matching the `read_file` limit.
const DIFF_MAX_BYTES: usize = 1024 * 1024;

/// Renders `/diff`: one diff card per file the file tools touched this
/// session, comparing the oldest `/undo` pre-image with the current contents.
pub(in crate::tui) fn show_diff(agent: &Agent, state: &mut ViewState) {
    let work_tree = crate::config::working_dir();
    show_diff_files(state, &work_tree, &agent.touched_files());
}

/// Renders diff cards for explicit touched-file records. Split from
/// `show_diff` so tests can drive real files without checkpoint plumbing.
pub(in crate::tui) fn show_diff_files(
    state: &mut ViewState,
    work_tree: &std::path::Path,
    files: &[crate::checkpoint::TouchedFile],
) {
    let mut skipped: Vec<String> = Vec::new();
    let mut shown = 0usize;
    for file in files {
        let display = diff_display_path(work_tree, &file.path);
        let previous = match (file.existed, &file.previous) {
            (true, Some(bytes)) => Some(bytes.clone()),
            (true, None) => {
                skipped.push(format!("{display} (pre-image unreadable)"));
                continue;
            }
            (false, _) => None,
        };
        let new_bytes = match std::fs::read(&file.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if previous.is_none() {
                    // Created and deleted again within the session.
                    continue;
                }
                Vec::new()
            }
            Err(_) => {
                skipped.push(format!("{display} (unreadable)"));
                continue;
            }
        };
        let old_bytes = previous.unwrap_or_default();
        if old_bytes == new_bytes {
            continue;
        }
        let (Ok(old), Ok(new)) = (String::from_utf8(old_bytes), String::from_utf8(new_bytes))
        else {
            skipped.push(format!("{display} (not UTF-8)"));
            continue;
        };
        if old.len() > DIFF_MAX_BYTES || new.len() > DIFF_MAX_BYTES {
            skipped.push(format!("{display} (over 1 MiB)"));
            continue;
        }
        state
            .transcript
            .push_diff(display, super::super::tool_view::edit_diff(&old, &new));
        shown += 1;
    }
    if shown == 0 && skipped.is_empty() {
        state.notice(
            "No file changes recorded this session. Changes made through `shell` or exec tools are not tracked.",
        );
        return;
    }
    if !skipped.is_empty() {
        let mut text = String::from("Files not shown\n\n");
        for path in &skipped {
            text.push_str(&format!("- {path}\n"));
        }
        state.notice(text);
    }
}

fn diff_display_path(work_tree: &std::path::Path, path: &str) -> String {
    std::path::Path::new(path)
        .strip_prefix(work_tree)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}
