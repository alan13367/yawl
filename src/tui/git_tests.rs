//! Git dashboard behavior regressions.

use super::super::{RenderCache, Transcript};
use super::*;

// Existing behavior tests wait for the public asynchronous action to settle.
fn handle_event(state: &mut ViewState, editor: &mut Editor, event: Event) {
    super::handle_event(state, editor, event);
    jobs::settle(state);
}

fn open_selected_diff(state: &mut ViewState) {
    super::open_selected_diff(state);
    jobs::settle(state);
}

fn confirm_action(state: &mut ViewState) {
    super::confirm_action(state);
    jobs::settle(state);
}

fn set_file_staged(state: &mut ViewState, index: usize, staged: bool) {
    super::set_file_staged(state, index, staged);
    jobs::settle(state);
}

fn poll_tick(state: &mut ViewState) -> bool {
    let mut changed = super::poll_tick(state);
    let deadline = Instant::now() + Duration::from_secs(10);
    while state.git_job.is_some() {
        assert!(Instant::now() < deadline);
        changed |= jobs::poll(state);
        std::thread::sleep(Duration::from_millis(1));
    }
    changed
}

fn test_state() -> ViewState {
    ViewState {
        transcript: Transcript::from_messages(&[]),
        tools_expanded: false,
        model: "test".into(),
        reasoning_effort: None,
        hide_reasoning: false,
        accent_color: UiColor::WHITE,
        selection_color: UiColor::WHITE,
        status_bar: Default::default(),
        status_bar_draft: None,
        show_scroll_bar: false,
        scroll_bar_enabled: false,
        scroll_bar_auto_hide: false,
        bell: false,
        scroll_bar_idle_ticks: 0,
        scroll_geometry: None,
        scroll_bar_drag: None,
        copy_toast_ticks: 0,
        spinner_tick: 0,
        turn_started: None,
        context_tokens: 0,
        context_window: 100,
        usage: crate::provider::UsageSummary::default(),
        activity: String::new(),
        scroll_offset: 0,
        queued_inputs: std::collections::VecDeque::new(),
        pending_steers: std::collections::VecDeque::new(),
        active_goal: None,
        goal_running: false,
        active_plan: None,
        plan_draft: false,
        pending_plan_implementation: false,
        question: None,
        pending_actions: std::collections::VecDeque::new(),
        completions: Vec::new(),
        completion_index: 0,
        completion_filter: None,
        file_index: crate::tui::files::FileIndex::default(),
        picker: None,
        connection: None,
        subagent_manager: crate::subagent::SubagentManager::new("test".into(), 3),
        subagent_snapshots: Vec::new(),
        subagent_tokens: 0,
        subagents_enabled: false,
        subagent_view: None,
        background_processes: crate::background::BackgroundProcessManager::default(),
        background_active_count: 0,
        process_view: None,
        git_view: None,
        git_job: None,
        git_init: None,
        render_cache: RenderCache::default(),
    }
}

fn state() -> ViewState {
    let mut state = test_state();
    state.git_view = Some(GitView::new(PathBuf::from("/unused"), GitStatus::default()));
    state
}

#[test]
fn browsing_multiple_diffs_exits_with_two_escapes() {
    let Some(repo) = Repo::new("escape-diffs") else {
        return;
    };
    for name in ["a.rs", "b.rs", "c.rs"] {
        std::fs::write(repo.0.join(name), "changed\n").unwrap();
    }
    let (status, _) = load_status(&repo.0).unwrap();
    let mut state = state();
    state.git_view = Some(GitView::new(repo.0.clone(), status));
    let mut editor = Editor::default();
    for index in [0, 1, 2, 0] {
        render(&mut state, &editor, 120, 30);
        let layout = &state.git_view.as_ref().unwrap().layout;
        let row = layout
            .file_rows
            .iter()
            .find(|(_, file)| *file == index)
            .unwrap()
            .0;
        let column = layout.divider + 5;
        handle_event(
            &mut state,
            &mut editor,
            Event::Mouse(MouseEvent {
                kind: MouseKind::Press,
                row,
                column,
            }),
        );
        assert!(state.git_view.as_ref().unwrap().diff.is_some());
    }
    handle_event(&mut state, &mut editor, Event::Key(Key::Escape));
    assert!(
        state.git_view.as_ref().unwrap().diff.is_none(),
        "first Escape must close the diff"
    );
    handle_event(&mut state, &mut editor, Event::Key(Key::Escape));
    assert!(state.git_view.is_none(), "second Escape must close /git");
}

#[test]
fn file_hover_moves_without_selecting_opening_or_staging() {
    for diff_open in [false, true] {
        let mut state = state();
        let status = parse_status_porcelain("## main\0 M a.rs\0 M b.rs\0 M c.rs\0");
        let mut view = GitView::new(PathBuf::from("/unused"), status);
        if diff_open {
            view.diff = Some(parse_unified_diff("a.rs", false, "-before\n+after\n"));
            view.focus = GitFocus::Diff;
        }
        state.git_view = Some(view);
        let editor = Editor::default();
        let (before, _) = render(&mut state, &editor, 120, 30);
        let layout = &state.git_view.as_ref().unwrap().layout;
        let row = layout.file_rows[1].0;
        let column = layout.divider + 5;
        handle_mouse(
            &mut state,
            MouseEvent {
                kind: MouseKind::Move,
                row,
                column,
            },
        );
        let (hovered, _) = render(&mut state, &editor, 120, 30);
        assert_ne!(before[row], hovered[row]);
        assert_eq!(
            markdown::strip_ansi(&before[row]),
            markdown::strip_ansi(&hovered[row])
        );
        assert!(pointer_over_control(&state));
        let view = state.git_view.as_ref().unwrap();
        assert_eq!(view.selected, 0);
        assert_eq!(view.hovered_file(), Some(1));
        assert_eq!(view.diff.is_some(), diff_open);
        // Moving over stage and discard controls must not invoke either action.
        let actions = view.layout.actions.clone();
        for hit in actions {
            handle_mouse(
                &mut state,
                MouseEvent {
                    kind: MouseKind::Move,
                    row: hit.row,
                    column: hit.x0,
                },
            );
            let view = state.git_view.as_ref().unwrap();
            assert!(pointer_over_control(&state));
            assert!(view.confirm.is_none());
            assert!(view.notice.is_empty());
            assert_eq!(view.selected, 0);
        }
        // Moving back into chat or the diff clears the highlight and pointer.
        handle_mouse(
            &mut state,
            MouseEvent {
                kind: MouseKind::Move,
                row,
                column: 5,
            },
        );
        assert!(!pointer_over_control(&state));
        assert_eq!(state.git_view.as_ref().unwrap().hovered_file(), None);
        let (after, _) = render(&mut state, &editor, 120, 30);
        assert_eq!(after[row], before[row]);
    }
}

#[test]
fn hover_uses_current_layout_and_never_targets_controls_behind_overlays() {
    let mut state = state();
    let status = parse_status_porcelain("## main\0 M a.rs\0 M b.rs\0");
    state.git_view = Some(GitView::new(PathBuf::from("/unused"), status));
    let editor = Editor::default();
    render(&mut state, &editor, 120, 30);
    let layout = &state.git_view.as_ref().unwrap().layout;
    let row = layout.file_rows[0].0;
    let column = layout.divider + 5;
    handle_mouse(
        &mut state,
        MouseEvent {
            kind: MouseKind::Move,
            row,
            column,
        },
    );
    assert_eq!(state.git_view.as_ref().unwrap().hovered_file(), Some(0));

    // Refreshing/reordering files beneath a stationary mouse follows the screen row.
    let view = state.git_view.as_mut().unwrap();
    view.status = parse_status_porcelain("## main\0M  staged.rs\0 M a.rs\0 M b.rs\0");
    view.flat = view.status.flat();
    render(&mut state, &editor, 120, 30);
    let view = state.git_view.as_ref().unwrap();
    assert_eq!(view.flat[view.hovered_file().unwrap()].path, "staged.rs");

    state.git_view.as_mut().unwrap().commit_menu = Some(CommitMenuState::default());
    render(&mut state, &editor, 120, 30);
    assert_eq!(state.git_view.as_ref().unwrap().hovered_file(), None);
    state.git_view.as_mut().unwrap().confirm = Some(Confirm::DiscardAll);
    render(&mut state, &editor, 120, 30);
    assert_eq!(state.git_view.as_ref().unwrap().mouse_target(), None);
    assert!(!pointer_over_control(&state));
    let hit = state.git_view.as_ref().unwrap().layout.confirm_rows[0];
    handle_mouse(
        &mut state,
        MouseEvent {
            kind: MouseKind::Move,
            row: hit.row,
            column: hit.x0,
        },
    );
    assert!(pointer_over_control(&state));
    assert!(state.git_view.as_ref().unwrap().confirm.is_some());
}

#[test]
fn hover_stops_at_panel_edges_and_clears_on_focus_loss() {
    let mut state = state();
    let status = parse_status_porcelain("## main\0 M a.rs\0 M b.rs\0");
    state.git_view = Some(GitView::new(PathBuf::from("/unused"), status));
    let mut editor = Editor::default();
    render(&mut state, &editor, 120, 30);
    let layout = &state.git_view.as_ref().unwrap().layout;
    let row = layout.file_rows[1].0;
    let divider = layout.divider;
    for column in [divider, 120] {
        handle_mouse(
            &mut state,
            MouseEvent {
                kind: MouseKind::Move,
                row,
                column,
            },
        );
        assert!(!pointer_over_control(&state));
    }
    handle_mouse(
        &mut state,
        MouseEvent {
            kind: MouseKind::Move,
            row,
            column: divider + 5,
        },
    );
    assert!(pointer_over_control(&state));
    handle_event(&mut state, &mut editor, Event::FocusLost);
    assert!(!pointer_over_control(&state));
    assert_eq!(state.git_view.as_ref().unwrap().hovered_file(), None);
    // Input fields keep the text cursor.
    handle_mouse(
        &mut state,
        MouseEvent {
            kind: MouseKind::Move,
            row: 1,
            column: divider + 5,
        },
    );
    assert!(!pointer_over_control(&state));
}

#[test]
fn visible_menu_arrow_opens_menu() {
    for columns in [40, 60, 80, 120, 200] {
        for amend in [false, true] {
            let mut state = state();
            let view = state.git_view.as_mut().unwrap();
            view.amend = amend;
            view.commit = "message must survive".into();
            let (frame, _) = render(&mut state, &Editor::default(), columns, 30);
            let plain = markdown::strip_ansi(&frame[4]);
            let arrow_byte = plain.find('∨').expect("menu arrow must be visible");
            let column = markdown::visible_width(&plain[..arrow_byte]);
            handle_mouse(
                &mut state,
                MouseEvent {
                    kind: MouseKind::Press,
                    row: 4,
                    column,
                },
            );
            let view = state.git_view.as_ref().unwrap();
            assert!(
                view.commit_menu.is_some(),
                "columns={columns}, amend={amend}: {}",
                view.notice
            );
            assert_eq!(view.commit, "message must survive");
            assert!(view.notice.is_empty());
        }
    }
}

#[test]
fn unstaged_header_plus_stages_all_changes() {
    let Some(repo) = Repo::new("stage-all-header") else {
        return;
    };
    std::fs::write(repo.0.join("tracked.txt"), "before\n").unwrap();
    run_git(&repo.0, &["add", "tracked.txt"]).unwrap();
    repo.commit();
    std::fs::write(repo.0.join("tracked.txt"), "after\n").unwrap();
    std::fs::write(repo.0.join("new.txt"), "new\n").unwrap();
    for columns in [60, 120] {
        let mut state = repo.state();
        let (frame, _) = render(&mut state, &Editor::default(), columns, 30);
        let (row, plain) = frame
            .iter()
            .enumerate()
            .map(|(row, line)| (row, markdown::strip_ansi(line)))
            .find(|(_, line)| line.contains("UNSTAGED"))
            .unwrap();
        let plus = plain.rfind('+').unwrap();
        let column = markdown::visible_width(&plain[..plus]);
        assert_eq!(
            state
                .git_view
                .as_ref()
                .unwrap()
                .layout
                .target(ScreenPoint { row, column }),
            Some(MouseTarget::StageAll)
        );
        handle_event(
            &mut state,
            &mut Editor::default(),
            Event::Mouse(MouseEvent {
                kind: MouseKind::Press,
                row,
                column,
            }),
        );
        let view = state.git_view.as_ref().unwrap();
        assert_eq!(view.status.staged.len(), 2);
        assert!(view.status.unstaged.is_empty());
        assert!(view.status.untracked.is_empty());
        render(&mut state, &Editor::default(), columns, 30);
        assert!(
            state
                .git_view
                .as_ref()
                .unwrap()
                .layout
                .stage_all_row
                .is_none()
        );
        run_git(&repo.0, &["reset"]).unwrap();
    }
}

#[test]
fn commit_message_box_is_labeled_and_focuses_on_click() {
    let mut state = state();
    let editor = Editor::default();
    let cells = |frame: &[String]| -> Vec<String> {
        frame
            .iter()
            .map(|line| {
                // Drop the transcript side past the first divider; the box
                // itself contains border cells.
                let stripped = markdown::strip_ansi(line);
                let mut parts = stripped.split('│');
                parts.next();
                parts.collect::<Vec<_>>().join("│")
            })
            .collect()
    };
    // Unfocused: a labeled box whose hint names the edit key (Enter here
    // opens diffs, so it must not promise a commit).
    let (frame, cursor) = render(&mut state, &editor, 120, 30);
    let right = cells(&frame);
    assert!(
        right[1].starts_with('╭') && right[1].contains("Message"),
        "labeled top border, got {:?}",
        right[1]
    );
    assert!(
        right[2].contains("(e to edit)"),
        "unfocused hint names the edit key, got {:?}",
        right[2]
    );
    assert!(
        right[3].starts_with('╰'),
        "bottom border, got {:?}",
        right[3]
    );
    assert!(frame[1].contains("\x1b[2m╭"), "unfocused border stays dim");
    assert_eq!(cursor, HIDDEN_CURSOR);

    // Clicking even the border focuses the input: the border accents, the
    // hint explains committing, and the text cursor parks on the input row.
    let divider = state.git_view.as_ref().unwrap().layout.divider;
    let mut editor = Editor::default();
    handle_event(
        &mut state,
        &mut editor,
        Event::Mouse(MouseEvent {
            kind: MouseKind::Press,
            row: 1,
            column: divider + 5,
        }),
    );
    assert_eq!(state.git_view.as_ref().unwrap().focus, GitFocus::Commit);
    let (frame, cursor) = render(&mut state, &editor, 120, 30);
    let right = cells(&frame);
    assert!(
        right[2].contains("Type a message"),
        "focused hint explains committing, got {:?}",
        right[2]
    );
    assert!(
        !frame[1].contains("\x1b[2m╭"),
        "focused border accents instead of dimming"
    );
    assert_eq!(cursor.0, COMMIT_BOX_INPUT + 1);
    let view = state.git_view.as_mut().unwrap();
    view.commit = "abcdef".into();
    view.commit_cursor = 2;
    let (frame, cursor) = render(&mut state, &editor, 120, 30);
    let input = markdown::strip_ansi(&frame[COMMIT_BOX_INPUT]);
    let text_start = input.find("abcdef").unwrap();
    assert_eq!(
        cursor,
        (
            COMMIT_BOX_INPUT + 1,
            markdown::visible_width(&input[..text_start]) + 3
        )
    );
}

#[test]
fn late_change_is_not_silently_hidden() {
    let mut output = String::from("@@ -1,3000 +1,3000 @@\n");
    for _ in 0..2500 {
        output.push_str(" unchanged\n");
    }
    output.push_str("-old\n+new\n");
    let diff = parse_unified_diff("large.rs", false, &output);
    assert!(diff.truncated);
    assert_eq!(diff.added, 0);
    let mut state = state();
    state.git_view.as_mut().unwrap().diff = Some(diff);
    let plain = markdown::strip_ansi(&render_diff_pane(&mut state, 80, 24).join("\n"));
    assert!(plain.contains("truncated"), "missing truncation warning");
}
#[test]
fn diff_header_sanitizes_filename_escapes() {
    let mut state = state();
    state.git_view.as_mut().unwrap().diff = Some(parse_unified_diff("file\x1b[2J.rs", false, ""));
    let rendered = render_diff_pane(&mut state, 80, 24).join("\n");
    assert!(
        !rendered.contains("\x1b[2J"),
        "filename injected erase-screen escape into frame"
    );
}
#[test]
fn diff_keeps_patch_like_content_lines() {
    let diff = parse_unified_diff("a.md", false, "@@ -1 +1 @@\n--- old\n+++ new\n");
    assert_eq!((diff.removed, diff.added), (1, 1));
}
#[test]
fn file_selection_stays_visible() {
    let status = parse_status_porcelain("## main\0 M a\0 M b\0 M c\0 M d\0 M e\0 M f\0");
    let mut view = GitView::new(PathBuf::from("/unused"), status);
    view.selected = 4;
    view.ensure_selection_visible(5);
    let area = file_area_rows(&view);
    let start = file_area_start(&view, &area, 5);
    assert!(
        area.iter()
            .skip(start)
            .take(5)
            .any(|row| matches!(row, FileAreaRow::File(i) if *i == view.selected)),
        "selected file is below rendered window"
    );
}

struct Repo(PathBuf);
impl Repo {
    fn new(name: &str) -> Option<Self> {
        if !git_available() {
            return None;
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "yawl-git-test-{name}-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        run_git(&root, &["init", "--template="]).unwrap();
        Some(Self(root))
    }
    fn commit(&self) {
        run_git(
            &self.0,
            &[
                "-c",
                "user.name=review",
                "-c",
                "user.email=review@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-m",
                "initial",
            ],
        )
        .unwrap();
    }
    fn state(&self) -> ViewState {
        let mut state = test_state();
        let (status, raw) = load_status(&self.0).unwrap();
        let mut view = GitView::new(self.0.clone(), status);
        view.last_status_raw = Some(raw);
        state.git_view = Some(view);
        state
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn poll_refreshes_already_dirty_diff() {
    let Some(repo) = Repo::new("poll") else {
        return;
    };
    std::fs::write(repo.0.join("note.txt"), "base\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    repo.commit();
    std::fs::write(repo.0.join("note.txt"), "edit one\n").unwrap();
    let mut state = repo.state();
    open_selected_diff(&mut state);
    std::fs::write(repo.0.join("note.txt"), "edit two\n").unwrap();
    assert!(poll_tick(&mut state));
    assert!(
        state
            .git_view
            .as_ref()
            .unwrap()
            .diff
            .as_ref()
            .unwrap()
            .lines
            .iter()
            .any(|l| l.text == "edit two"),
        "diff retains first edit"
    );
}
#[test]
fn discard_untracked_directory_preserves_ignored_files() {
    let Some(repo) = Repo::new("ignored") else {
        return;
    };
    std::fs::write(repo.0.join(".gitignore"), "*.secret\n").unwrap();
    run_git(&repo.0, &["add", ".gitignore"]).unwrap();
    repo.commit();
    std::fs::create_dir(repo.0.join("newdir")).unwrap();
    std::fs::write(repo.0.join("newdir/public.txt"), "untracked").unwrap();
    std::fs::write(repo.0.join("newdir/local.secret"), "ignored").unwrap();
    let mut state = repo.state();
    assert_eq!(state.git_view.as_ref().unwrap().flat[0].path, "newdir/");
    handle_key(&mut state, Key::Char('d'));
    confirm_action(&mut state);
    assert!(!repo.0.join("newdir/public.txt").exists());
    assert!(
        repo.0.join("newdir/local.secret").exists(),
        "discard removed ignored data absent from status"
    );
}
#[test]
fn unstage_before_first_commit() {
    let Some(repo) = Repo::new("unborn") else {
        return;
    };
    std::fs::write(repo.0.join("note.txt"), "new\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    let mut state = repo.state();
    set_file_staged(&mut state, 0, false);
    let view = state.git_view.as_ref().unwrap();
    assert!(
        view.status.staged.is_empty(),
        "unstage failed: {}",
        view.notice
    );
}

#[test]
fn repository_text_cannot_inject_terminal_controls() {
    let hostile = "name\x1b[2J\x1b]52;c;dGVzdA==\x07\r\x08";
    let mut state = state();
    let view = state.git_view.as_mut().unwrap();
    view.status.branch = hostile.into();
    view.status.upstream = Some(hostile.into());
    view.status.untracked.push(GitFile {
        path: hostile.into(),
        x: '?',
        y: '?',
        section: GitSection::Untracked,
        renamed_from: Some(hostile.into()),
    });
    view.flat = view.status.flat();
    view.commit = hostile.into();
    view.notice = hostile.into();
    view.history.push(HistoryEntry {
        short: hostile.into(),
        subject: hostile.into(),
        author: hostile.into(),
        date: hostile.into(),
        refs: hostile.into(),
        ..HistoryEntry::default()
    });
    for confirm in [false, true] {
        if confirm {
            handle_key(&mut state, Key::Char('d'));
        }
        let (frame, _) = render(&mut state, &Editor::default(), 200, 40);
        assert_plain_repository_text(&frame);
    }
    let view = state.git_view.as_mut().unwrap();
    view.confirm = None;
    view.show_log = true;
    assert_plain_repository_text(&render_log(&mut state, 200, 30));
    let view = state.git_view.as_mut().unwrap();
    view.branches.push(hostile.into());
    assert_plain_repository_text(&render_branches(&mut state, 200, 30));
    // Rendering must not modify the paths used for Git operations.
    assert_eq!(state.git_view.as_ref().unwrap().flat[0].path, hostile);

    let mut flow = GitInitFlow::new(PathBuf::from(hostile));
    flow.remote = hostile.into();
    flow.error = Some(hostile.into());
    state.git_init = Some(flow);
    assert_plain_repository_text(&render_init(&state, &Editor::default(), 200, 30).0);
}

fn assert_plain_repository_text(lines: &[String]) {
    for line in lines {
        assert!(!line.contains("\x1b[2J"));
        assert!(!line.contains("\x1b]52"));
        assert!(!markdown::strip_ansi(line).chars().any(char::is_control));
    }
}

#[test]
fn selection_stays_visible_across_headers_and_resizes() {
    let mut state = state();
    let view = state.git_view.as_mut().unwrap();
    view.status = parse_status_porcelain("## main\0M  a\0MM b\0 M c\0?? d\0?? e\0");
    view.flat = view.status.flat();
    let count = view.flat.len();
    for rows in [10, 12, 16, 24] {
        for selected in (0..count).chain((0..count).rev()) {
            state.git_view.as_mut().unwrap().selected = selected;
            render(&mut state, &Editor::default(), 80, rows);
            let view = state.git_view.as_ref().unwrap();
            assert!(
                view.layout
                    .file_rows
                    .iter()
                    .any(|(_, index)| *index == selected),
                "selected {selected} absent at height {rows}"
            );
        }
    }
}

#[test]
fn diff_headers_between_files_do_not_become_content() {
    let output = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n--- old\n+++ new\n tail\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -5 +5 @@\n-x\n+y\n";
    let diff = parse_unified_diff("commit", false, output);
    assert_eq!((diff.removed, diff.added), (2, 2));
    assert_eq!(diff.lines[3].old_no, Some(2));
    assert_eq!(diff.lines[3].new_no, Some(2));
    assert_eq!(diff.lines.last().unwrap().new_no, Some(5));
}

#[test]
fn late_changes_remain_visible_in_worktree_index_and_history() {
    let Some(repo) = Repo::new("late-diff") else {
        return;
    };
    let base = "unchanged\n".repeat(3000);
    std::fs::write(repo.0.join("large.txt"), &base).unwrap();
    run_git(&repo.0, &["add", "large.txt"]).unwrap();
    repo.commit();
    let edited = format!("{}changed\n", "unchanged\n".repeat(2999));
    std::fs::write(repo.0.join("large.txt"), edited).unwrap();
    for staged in [false, true] {
        if staged {
            run_git(&repo.0, &["add", "large.txt"]).unwrap();
        }
        let state = repo.state();
        let view = state.git_view.as_ref().unwrap();
        let diff = load_diff(&repo.0, &view.flat[0]).unwrap();
        assert_late_change(&diff);
        assert_eq!(diff.staged, staged);
    }
    repo.commit();
    let entry = load_history_limit(&repo.0, INITIAL_HISTORY_LIMIT)
        .0
        .into_iter()
        .next()
        .unwrap();
    assert_late_change(&load_commit_diff(&repo.0, &entry).unwrap());
}

fn assert_late_change(diff: &LoadedDiff) {
    assert!(!diff.truncated);
    assert_eq!((diff.added, diff.removed), (1, 1));
    assert!(
        diff.lines
            .iter()
            .any(|line| line.text == "changed" && line.new_no == Some(3000))
    );
}

#[test]
fn oversized_untracked_diff_reports_truncation_instead_of_no_changes() {
    let Some(repo) = Repo::new("oversize") else {
        return;
    };
    std::fs::write(repo.0.join("large.txt"), "a".repeat(MAX_DIFF_BYTES + 1)).unwrap();
    let mut state = repo.state();
    open_selected_diff(&mut state);
    let lines = render_diff_pane(&mut state, 80, 24);
    let plain = markdown::strip_ansi(&lines.join("\n"));
    assert!(plain.contains("truncated"));
    assert!(!plain.contains("No content changes"));
}

#[test]
fn poll_refreshes_staged_and_untracked_content_without_resetting_edits() {
    for staged in [false, true] {
        let Some(repo) = Repo::new("poll-content") else {
            return;
        };
        std::fs::write(repo.0.join("note.txt"), "one\n").unwrap();
        if staged {
            run_git(&repo.0, &["add", "note.txt"]).unwrap();
        }
        let mut state = repo.state();
        open_selected_diff(&mut state);
        let view = state.git_view.as_mut().unwrap();
        view.commit = "keep message".into();
        view.diff.as_mut().unwrap().scroll = 1;
        let old_status = view.last_status_raw.clone();
        std::fs::write(repo.0.join("note.txt"), "two\n").unwrap();
        if staged {
            run_git(&repo.0, &["add", "note.txt"]).unwrap();
        }
        assert_eq!(
            old_status.as_deref(),
            Some(run_git(&repo.0, STATUS_ARGS).unwrap().as_str())
        );
        assert!(poll_tick(&mut state));
        let view = state.git_view.as_mut().unwrap();
        assert_eq!(view.commit, "keep message");
        let diff = view.diff.as_ref().unwrap();
        assert_eq!(diff.scroll, 1);
        assert!(diff.lines.iter().any(|line| line.text == "two"));
        view.last_poll = None;
        assert!(
            !poll_tick(&mut state),
            "unchanged diff must not request a redraw"
        );
    }
}

#[test]
fn unstage_all_before_first_commit_preserves_files() {
    let Some(repo) = Repo::new("unborn-all") else {
        return;
    };
    for path in ["a.txt", "b.txt"] {
        std::fs::write(repo.0.join(path), "new\n").unwrap();
    }
    run_git(&repo.0, &["add", "-A"]).unwrap();
    let mut state = repo.state();
    handle_key(&mut state, Key::Char('u'));
    jobs::settle(&mut state);
    assert!(state.git_view.as_ref().unwrap().status.staged.is_empty());
    for path in ["a.txt", "b.txt"] {
        assert_eq!(std::fs::read_to_string(repo.0.join(path)).unwrap(), "new\n");
    }
}

#[test]
fn unstage_rename_preserves_both_working_paths_and_other_staging() {
    let Some(repo) = Repo::new("reset-rename") else {
        return;
    };
    std::fs::write(repo.0.join("old.txt"), "content\n").unwrap();
    run_git(&repo.0, &["add", "old.txt"]).unwrap();
    repo.commit();
    run_git(&repo.0, &["mv", "old.txt", "new.txt"]).unwrap();
    std::fs::write(repo.0.join("other.txt"), "keep staged\n").unwrap();
    run_git(&repo.0, &["add", "other.txt"]).unwrap();
    let mut state = repo.state();
    let index = state
        .git_view
        .as_ref()
        .unwrap()
        .flat
        .iter()
        .position(|file| file.path == "new.txt")
        .unwrap();
    set_file_staged(&mut state, index, false);
    let view = state.git_view.as_ref().unwrap();
    assert_eq!(view.status.staged.len(), 1);
    assert_eq!(view.status.staged[0].path, "other.txt");
    assert!(!repo.0.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(repo.0.join("new.txt")).unwrap(),
        "content\n"
    );
}

#[test]
fn discard_targets_literal_paths_and_preserves_files_staged_since_confirmation() {
    let Some(repo) = Repo::new("clean-literal") else {
        return;
    };
    for path in ["*.txt", "other.txt"] {
        std::fs::write(repo.0.join(path), "untracked\n").unwrap();
    }
    let mut state = repo.state();
    let index = state
        .git_view
        .as_ref()
        .unwrap()
        .flat
        .iter()
        .position(|file| file.path == "*.txt")
        .unwrap();
    state.git_view.as_mut().unwrap().selected = index;
    handle_key(&mut state, Key::Char('d'));
    confirm_action(&mut state);
    assert!(!repo.0.join("*.txt").exists());
    assert!(repo.0.join("other.txt").exists());
    handle_key(&mut state, Key::Char('d'));
    run_git(&repo.0, &["add", "other.txt"]).unwrap();
    confirm_action(&mut state);
    assert!(repo.0.join("other.txt").exists());
}

#[test]
fn bounded_capture_still_drains_the_pipe() {
    let mut pipe = std::io::Cursor::new(vec![b'x'; MAX_DIFF_BYTES * 2]);
    let captured = drain_pipe(Some(&mut pipe), 256);
    assert_eq!(captured, vec![b'x'; 256]);
    assert_eq!(pipe.position(), (MAX_DIFF_BYTES * 2) as u64);
}

#[test]
fn oversized_git_output_is_bounded_and_marked_partial() {
    let Some(repo) = Repo::new("bounded-diff") else {
        return;
    };
    std::fs::write(repo.0.join("large.txt"), "line\n".repeat(MAX_DIFF_BYTES)).unwrap();
    run_git(&repo.0, &["add", "large.txt"]).unwrap();
    let output =
        run_git_files(&repo.0, "diff", &["--cached", "--no-color"], &["large.txt"]).unwrap();
    assert_eq!(output.len(), MAX_DIFF_BYTES + 1);
    let state = repo.state();
    let diff = load_diff(&repo.0, &state.git_view.as_ref().unwrap().flat[0]).unwrap();
    assert!(diff.truncated);
    assert!(diff.lines.len() <= MAX_DIFF_LINES);
}

#[test]
fn background_result_preserves_commit_edits_and_navigation() {
    let mut state = state();
    state.git_view.as_mut().unwrap().commit = "old message".into();
    let (release, ready) = std::sync::mpsc::channel();
    assert!(jobs::start(&mut state, "test action", move |state| {
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        let view = state.git_view.as_mut().unwrap();
        view.commit.clear();
        view.commit_cursor = 0;
        view.status.branch = "updated branch".into();
    }));
    let view = state.git_view.as_mut().unwrap();
    view.focus = GitFocus::Commit;
    view.commit = "next message".into();
    view.commit_cursor = 12;
    release.send(()).unwrap();
    jobs::settle(&mut state);
    let view = state.git_view.as_ref().unwrap();
    assert_eq!(view.status.branch, "updated branch");
    assert_eq!(view.commit, "next message");
    assert_eq!(view.commit_cursor, 12);
    assert_eq!(view.focus, GitFocus::Commit);
}

#[test]
fn closing_dashboard_drops_late_results() {
    let mut state = state();
    let (release, ready) = std::sync::mpsc::channel();
    jobs::start(&mut state, "test action", move |_| {
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
    });
    state.git_view = None;
    release.send(()).unwrap();
    jobs::settle(&mut state);
    assert!(state.git_view.is_none());
}

#[test]
fn explicit_action_supersedes_read_only_poll() {
    let mut state = state();
    let (release, ready) = std::sync::mpsc::channel();
    jobs::start(&mut state, "", move |state| {
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        state.git_view.as_mut().unwrap().commit = "obsolete".into();
    });
    assert!(jobs::start(&mut state, "action", |state| {
        state.git_view.as_mut().unwrap().commit = "current".into();
    }));
    release.send(()).unwrap();
    jobs::settle(&mut state);
    assert_eq!(state.git_view.as_ref().unwrap().commit, "current");
}

#[test]
fn canceling_git_also_stops_children_holding_output_pipes() {
    let Some(repo) = Repo::new("cancel-worker") else {
        return;
    };
    let root = repo.0.clone();
    let token = crate::cancellation::CancellationToken::default();
    let worker_token = token.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let result = crate::cancellation::scope(&worker_token, || {
            run_git(
                &root,
                &[
                    "-c",
                    "alias.yawl-wait=!touch worker-started; sleep 10",
                    "yawl-wait",
                ],
            )
        });
        tx.send(result).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    while !repo.0.join("worker-started").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    token.cancel();
    let result = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("cancellation must release the child and both pipes");
    handle.join().unwrap();
    assert!(result.unwrap_err().contains("cancelled"));
}

#[test]
fn wheel_up_moves_git_lists_toward_the_top() {
    // Button 64 (wheel-up) decodes to +3, button 65 (wheel-down) to -3. All
    // git offsets count from the top, so wheel-up must shrink them.
    let mut state = state();
    let mut editor = Editor::default();
    let view = state.git_view.as_mut().unwrap();
    view.status =
        parse_status_porcelain("## main\0 M a.rs\0 M b.rs\0 M c.rs\0 M d.rs\0 M e.rs\0 M f.rs\0");
    view.flat = view.status.flat();
    view.file_scroll = 5;
    view.focus = GitFocus::Files;
    handle_event(&mut state, &mut editor, Event::MouseScroll(3));
    assert_eq!(state.git_view.as_ref().unwrap().file_scroll, 2);
    handle_event(&mut state, &mut editor, Event::MouseScroll(-3));
    assert_eq!(state.git_view.as_ref().unwrap().file_scroll, 5);

    let view = state.git_view.as_mut().unwrap();
    view.focus = GitFocus::History;
    view.history = (0..10)
        .map(|index| HistoryEntry {
            hash: format!("hash{index}"),
            short: format!("{index:07x}"),
            subject: format!("commit {index}"),
            ..HistoryEntry::default()
        })
        .collect();
    view.history_scroll = 5;
    handle_event(&mut state, &mut editor, Event::MouseScroll(3));
    assert_eq!(state.git_view.as_ref().unwrap().history_scroll, 2);
    handle_event(&mut state, &mut editor, Event::MouseScroll(-3));
    assert_eq!(state.git_view.as_ref().unwrap().history_scroll, 5);

    let view = state.git_view.as_mut().unwrap();
    view.focus = GitFocus::Diff;
    view.diff = Some(parse_unified_diff(
        "a.rs",
        false,
        "@@ -1 +1 @@\n-old\n+new\n",
    ));
    view.diff.as_mut().unwrap().scroll = 5;
    handle_event(&mut state, &mut editor, Event::MouseScroll(3));
    assert_eq!(
        state
            .git_view
            .as_ref()
            .unwrap()
            .diff
            .as_ref()
            .unwrap()
            .scroll,
        2
    );
    handle_event(&mut state, &mut editor, Event::MouseScroll(-3));
    assert_eq!(
        state
            .git_view
            .as_ref()
            .unwrap()
            .diff
            .as_ref()
            .unwrap()
            .scroll,
        5
    );

    let view = state.git_view.as_mut().unwrap();
    view.focus = GitFocus::Files;
    view.show_log = true;
    view.log_scroll = 5;
    handle_event(&mut state, &mut editor, Event::MouseScroll(3));
    assert_eq!(state.git_view.as_ref().unwrap().log_scroll, 2);
    handle_event(&mut state, &mut editor, Event::MouseScroll(-3));
    assert_eq!(state.git_view.as_ref().unwrap().log_scroll, 5);
}

#[test]
fn clicking_a_commit_opens_its_diff_like_a_file() {
    let Some(repo) = Repo::new("history-click") else {
        return;
    };
    std::fs::write(repo.0.join("note.txt"), "v1\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    repo.commit();
    std::fs::write(repo.0.join("note.txt"), "v2\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    repo.commit();
    let mut state = repo.state();
    let view = state.git_view.as_mut().unwrap();
    let (history, has_more) = load_history_limit(&repo.0, 10);
    view.history = history;
    view.history_has_more = has_more;
    view.focus = GitFocus::History;
    assert!(state.git_view.as_ref().unwrap().diff.is_none());
    render(&mut state, &Editor::default(), 120, 30);
    let layout = state.git_view.as_ref().unwrap().layout.clone();
    assert!(
        !layout.history_rows.is_empty(),
        "history rows must be clickable"
    );
    let (row, _) = layout.history_rows[0];
    let column = layout.divider + 5;
    let mut editor = Editor::default();
    handle_event(
        &mut state,
        &mut editor,
        Event::Mouse(MouseEvent {
            kind: MouseKind::Press,
            row,
            column,
        }),
    );
    let view = state.git_view.as_ref().unwrap();
    assert_eq!(view.focus, GitFocus::Diff, "commit click focuses the diff");
    assert!(
        view.diff.as_ref().is_some_and(|diff| diff.commit.is_some()),
        "commit click loads the commit diff, got {:?}",
        view.diff.as_ref().map(|diff| &diff.path)
    );
}

#[test]
fn open_commit_diff_owns_selection_and_clears_file_highlight() {
    let Some(repo) = Repo::new("history-owns-selection") else {
        return;
    };
    std::fs::write(repo.0.join("note.txt"), "v1\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    repo.commit();
    std::fs::write(repo.0.join("note.txt"), "v2\n").unwrap();
    run_git(&repo.0, &["add", "note.txt"]).unwrap();
    repo.commit();
    // Dirty worktree on top of two commits: one selectable file row plus two
    // clickable history rows.
    std::fs::write(repo.0.join("note.txt"), "dirty\n").unwrap();
    let mut state = repo.state();
    assert_eq!(state.git_view.as_ref().unwrap().flat.len(), 1);
    let view = state.git_view.as_mut().unwrap();
    let (history, has_more) = load_history_limit(&repo.0, 10);
    assert_eq!(history.len(), 2);
    view.history = history;
    view.history_has_more = has_more;
    view.focus = GitFocus::History;
    render(&mut state, &Editor::default(), 120, 30);
    let layout = state.git_view.as_ref().unwrap().layout.clone();
    assert_eq!(layout.history_rows.len(), 2);
    assert_eq!(layout.file_rows.len(), 1);
    // Click the older commit (history index 1).
    let (row, index) = layout.history_rows[1];
    assert_eq!(index, 1);
    let column = layout.divider + 5;
    let mut editor = Editor::default();
    handle_event(
        &mut state,
        &mut editor,
        Event::Mouse(MouseEvent {
            kind: MouseKind::Press,
            row,
            column,
        }),
    );
    let view = state.git_view.as_ref().unwrap();
    assert_eq!(view.focus, GitFocus::Diff);
    let opened_hash = view.history[index].hash.clone();
    let opened_short = view.history[index].short.clone();
    assert_eq!(
        view.diff.as_ref().and_then(|diff| diff.commit.as_deref()),
        Some(opened_hash.as_str()),
        "the diff must belong to the clicked commit"
    );
    // The open commit stays marked selected even though focus left the
    // history list, and no file row pretends its diff is open.
    let (frame, _) = render(&mut state, &Editor::default(), 120, 30);
    let right: Vec<String> = frame
        .iter()
        .map(|line| {
            markdown::strip_ansi(line)
                .split('│')
                .next_back()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let commit_row = right
        .iter()
        .find(|line| line.contains(opened_short.as_str()))
        .expect("clicked commit must be visible");
    assert!(
        commit_row.starts_with('›'),
        "open commit stays selected, got {commit_row:?}"
    );
    for line in right.iter().filter(|line| line.contains("note.txt")) {
        assert!(
            !line.starts_with('›'),
            "file highlight must clear while a commit diff is open, got {line:?}"
        );
    }
}

#[test]
fn wrapped_diff_tokens_keep_their_style_when_scrolled() {
    for kind in [DiffKind::Added, DiffKind::Removed, DiffKind::Context] {
        for (path, text, style) in [
            (
                "a.rs",
                "// for true 123 let return comment continues here",
                "\x1b[2;36m",
            ),
            (
                "a.py",
                "# for True 123 def return comment continues here",
                "\x1b[2;36m",
            ),
            (
                "a.rs",
                "\"for true 123 let return string continues here\"",
                "\x1b[32m",
            ),
        ] {
            let mut state = state();
            let mut diff = parse_unified_diff(path, false, "@@ -1 +1 @@\n-old\n+new\n");
            diff.lines = vec![DiffLine {
                old_no: Some(1),
                new_no: Some(1),
                kind,
                text: text.into(),
            }];
            diff.scroll = 1;
            state.git_view.as_mut().unwrap().diff = Some(diff);
            // Start after the comment marker or opening quote has scrolled
            // out of view. Both visible continuations must retain its style.
            let rows = render_diff_pane(&mut state, 24, 3);
            for row in &rows[1..] {
                assert!(row.contains(style), "missing token style in {row:?}");
                assert!(
                    !row.contains("\x1b[1;34m"),
                    "token became a keyword: {row:?}"
                );
                assert!(!row.contains("\x1b[35m"), "token became a number: {row:?}");
                assert!(markdown::strip_ansi(row).starts_with(&" ".repeat(12)));
            }
        }
    }
}

#[test]
fn diff_pane_wraps_long_lines_and_anchors_first_change() {
    let mut state = state();
    let long = "x".repeat(100);
    let view = state.git_view.as_mut().unwrap();
    view.diff = Some(LoadedDiff {
        path: "a.rs".into(),
        staged: false,
        untracked: false,
        commit: None,
        lines: vec![DiffLine {
            old_no: None,
            new_no: Some(1),
            kind: DiffKind::Added,
            text: long.clone(),
        }],
        added: 1,
        removed: 0,
        scroll: SCROLL_ANCHOR_PENDING,
        binary: false,
        truncated: false,
    });
    // Width 40 leaves code width 28: 100 chars wrap onto four screen rows.
    let lines = render_diff_pane(&mut state, 40, 24);
    assert_eq!(
        state
            .git_view
            .as_ref()
            .unwrap()
            .diff
            .as_ref()
            .unwrap()
            .scroll,
        0,
        "a lone first change anchors to the top"
    );
    let body: Vec<String> = lines
        .iter()
        .skip(1)
        .take(4)
        .map(|line| markdown::strip_ansi(line))
        .collect();
    assert_eq!(body.len(), 4);
    assert!(body[0].contains('1') && body[0].contains('+'));
    for continuation in &body[1..] {
        assert!(
            !continuation.chars().take(12).any(|c| c.is_ascii_digit()),
            "wrapped continuations show no numbers, got {continuation:?}"
        );
    }
    let text: String = body
        .iter()
        .map(|row| {
            row.chars()
                .skip(12)
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    assert_eq!(text, long);

    // Visual scrolling can land mid-line: the continuation keeps a blank
    // gutter instead of repeating the number. Shrink the pane so the scroll
    // offset takes effect (body height 3 of 4 wrapped rows).
    state
        .git_view
        .as_mut()
        .unwrap()
        .diff
        .as_mut()
        .unwrap()
        .scroll = 1;
    let lines = render_diff_pane(&mut state, 40, 4);
    let first_body = markdown::strip_ansi(&lines[1]);
    assert!(!first_body.chars().take(12).any(|c| c.is_ascii_digit()));
    assert_eq!(&first_body[12..], "x".repeat(28));

    // Skip an entire source line, then begin within a wrapped line.
    let diff = state.git_view.as_mut().unwrap().diff.as_mut().unwrap();
    diff.lines.insert(
        0,
        DiffLine {
            old_no: Some(1),
            new_no: Some(1),
            kind: DiffKind::Context,
            text: "offscreen".into(),
        },
    );
    diff.scroll = 2;
    let lines = render_diff_pane(&mut state, 40, 3);
    assert_eq!(lines.len(), 3);
    for row in &lines[1..] {
        assert_eq!(
            markdown::strip_ansi(row),
            format!("{}{}", " ".repeat(12), "x".repeat(28))
        );
    }
}

#[test]
fn scrolling_near_oldest_history_pages_in_more_commits() {
    let Some(repo) = Repo::new("history-pages") else {
        return;
    };
    for index in 0..5 {
        std::fs::write(repo.0.join("note.txt"), format!("v{index}\n")).unwrap();
        run_git(&repo.0, &["add", "note.txt"]).unwrap();
        repo.commit();
    }
    let mut state = repo.state();
    let view = state.git_view.as_mut().unwrap();
    let (history, _) = load_history_limit(&repo.0, 2);
    view.history = history;
    view.history_limit = 2;
    view.history_has_more = true;
    view.history_selected = 1;
    view.focus = GitFocus::History;
    maybe_load_more_history(&mut state);
    jobs::settle(&mut state);
    let view = state.git_view.as_ref().unwrap();
    assert!(
        view.history.len() > 2,
        "paging must grow beyond the initial limit, got {}",
        view.history.len()
    );
    assert!(
        view.history_limit > 2,
        "the limit must grow so refreshes keep the deeper history"
    );
    assert_eq!(view.history.len(), 5);
    assert!(
        !view.history_has_more,
        "a short page proves the end of history"
    );
}
