//! Background shell process dashboard and live log viewer.

use std::time::Instant;

use crate::background::{
    BackgroundProcessManager, BackgroundSnapshot, BackgroundStatus, OutputStream,
};
use crate::subagent::format_duration;

use super::dashboard::{
    Alignment, FieldColumn, PinnedScroll, TableChrome, reconcile_selection, render_table,
};
use super::events::{Event, Key};
use super::render::{HIDDEN_CURSOR, status_style};
use super::{ViewState, markdown};

pub(super) struct ProcessView {
    manager: BackgroundProcessManager,
    snapshots: Vec<BackgroundSnapshot>,
    mode: ProcessMode,
}

enum ProcessMode {
    Dashboard {
        selected_id: Option<String>,
        selected_index: usize,
        confirm_remove: bool,
    },
    Logs {
        id: String,
        scroll: PinnedScroll,
    },
}

pub(super) fn open_dashboard(state: &mut ViewState, manager: BackgroundProcessManager) {
    let snapshots = manager.snapshots();
    if snapshots.is_empty() {
        state.notice(
            "No background shell commands are tracked. The dashboard opens after the agent starts one.",
        );
        return;
    }
    let selected_id = snapshots.first().map(|snapshot| snapshot.id.to_string());
    state.process_view = Some(ProcessView {
        manager,
        snapshots,
        mode: ProcessMode::Dashboard {
            selected_id,
            selected_index: 0,
            confirm_remove: false,
        },
    });
    state.subagent_view = None;
    state.picker = None;
}

fn snapshot_id(snapshot: &BackgroundSnapshot) -> &str {
    snapshot.id.as_str()
}

pub(super) fn refresh(state: &mut ViewState) {
    let Some(view) = state.process_view.as_mut() else {
        return;
    };
    view.snapshots = view.manager.snapshots();
    if let ProcessMode::Dashboard {
        selected_id,
        selected_index,
        ..
    } = &mut view.mode
    {
        reconcile_selection(&view.snapshots, snapshot_id, selected_id, selected_index);
    }
}

pub(super) fn handle_event(state: &mut ViewState, event: Event) {
    refresh(state);
    let Some(mut view) = state.process_view.take() else {
        return;
    };
    let mode = std::mem::replace(
        &mut view.mode,
        ProcessMode::Dashboard {
            selected_id: None,
            selected_index: 0,
            confirm_remove: false,
        },
    );
    let next = match mode {
        ProcessMode::Dashboard {
            mut selected_id,
            mut selected_index,
            mut confirm_remove,
        } => {
            if confirm_remove {
                match event {
                    Event::Key(Key::Enter) => {
                        if let Some(id) = selected_id.as_deref()
                            && let Err(error) = view.manager.remove(id)
                        {
                            state.notice(error);
                        }
                        confirm_remove = false;
                    }
                    Event::Key(Key::Escape | Key::Ctrl('c')) => confirm_remove = false,
                    _ => {}
                }
                Some(ProcessMode::Dashboard {
                    selected_id,
                    selected_index,
                    confirm_remove,
                })
            } else {
                match event {
                    Event::Key(Key::Escape) => None,
                    Event::Key(Key::Enter) => selected_id.clone().map(|id| ProcessMode::Logs {
                        id,
                        scroll: PinnedScroll::default(),
                    }),
                    Event::Key(Key::Up | Key::Char('k')) => {
                        selected_index = selected_index.saturating_sub(1);
                        select_index(&view.snapshots, &mut selected_id, selected_index);
                        Some(ProcessMode::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_remove: false,
                        })
                    }
                    Event::Key(Key::Down | Key::Char('j')) => {
                        selected_index =
                            (selected_index + 1).min(view.snapshots.len().saturating_sub(1));
                        select_index(&view.snapshots, &mut selected_id, selected_index);
                        Some(ProcessMode::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_remove: false,
                        })
                    }
                    Event::Key(Key::Char('x') | Key::Ctrl('c'))
                        if selected(&view.snapshots, selected_id.as_deref())
                            .is_some_and(|snapshot| snapshot.status.is_active()) =>
                    {
                        if let Some(id) = selected_id.as_deref()
                            && let Err(error) = view.manager.stop(id)
                        {
                            state.notice(error);
                        }
                        Some(ProcessMode::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_remove: false,
                        })
                    }
                    Event::Key(Key::Char('r'))
                        if selected(&view.snapshots, selected_id.as_deref())
                            .is_some_and(|snapshot| !snapshot.status.is_active()) =>
                    {
                        if let Some(id) = selected_id.as_deref() {
                            match view.manager.restart(id) {
                                Ok(started) => selected_id = Some(started.id.to_string()),
                                Err(error) => state.notice(error),
                            }
                        }
                        view.snapshots = view.manager.snapshots();
                        reconcile_selection(
                            &view.snapshots,
                            snapshot_id,
                            &mut selected_id,
                            &mut selected_index,
                        );
                        Some(ProcessMode::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_remove: false,
                        })
                    }
                    Event::Key(Key::Char('d') | Key::Delete)
                        if selected(&view.snapshots, selected_id.as_deref())
                            .is_some_and(|snapshot| !snapshot.status.is_active()) =>
                    {
                        Some(ProcessMode::Dashboard {
                            selected_id,
                            selected_index,
                            confirm_remove: true,
                        })
                    }
                    _ => Some(ProcessMode::Dashboard {
                        selected_id,
                        selected_index,
                        confirm_remove: false,
                    }),
                }
            }
        }
        ProcessMode::Logs { id, mut scroll } => match event {
            Event::Key(Key::Escape) => Some(ProcessMode::Dashboard {
                selected_id: Some(id),
                selected_index: 0,
                confirm_remove: false,
            }),
            Event::Key(Key::Ctrl('c') | Key::Char('x'))
                if selected(&view.snapshots, Some(&id))
                    .is_some_and(|snapshot| snapshot.status.is_active()) =>
            {
                if let Err(error) = view.manager.stop(&id) {
                    state.notice(error);
                }
                Some(ProcessMode::Logs { id, scroll })
            }
            Event::Key(Key::Up) | Event::MouseScroll(1..) => {
                scroll.up(1);
                Some(ProcessMode::Logs { id, scroll })
            }
            Event::Key(Key::PageUp) => {
                scroll.up(10);
                Some(ProcessMode::Logs { id, scroll })
            }
            Event::Key(Key::Down) | Event::MouseScroll(..=-1) => {
                scroll.down(1);
                Some(ProcessMode::Logs { id, scroll })
            }
            Event::Key(Key::PageDown) => {
                scroll.down(10);
                Some(ProcessMode::Logs { id, scroll })
            }
            _ => Some(ProcessMode::Logs { id, scroll }),
        },
    };
    if let Some(mode) = next {
        view.mode = mode;
        view.snapshots = view.manager.snapshots();
        state.process_view = Some(view);
    }
}

/// Routes SIGINT from the terminal event loop to the selected process.
/// Returns whether the process view consumed the interrupt.
pub(super) fn handle_interrupt(state: &mut ViewState) -> bool {
    if state.process_view.is_none() {
        return false;
    }
    handle_event(state, Event::Key(Key::Ctrl('c')));
    true
}

fn select_index(
    snapshots: &[BackgroundSnapshot],
    selected_id: &mut Option<String>,
    selected_index: usize,
) {
    if let Some(snapshot) = snapshots.get(selected_index) {
        *selected_id = Some(snapshot.id.to_string());
    }
}

fn selected<'a>(
    snapshots: &'a [BackgroundSnapshot],
    id: Option<&str>,
) -> Option<&'a BackgroundSnapshot> {
    id.and_then(|id| snapshots.iter().find(|snapshot| snapshot.id.as_str() == id))
}

pub(super) fn render(
    state: &mut ViewState,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let view = state
        .process_view
        .as_mut()
        .expect("process rendering requires an active view");
    match &mut view.mode {
        ProcessMode::Dashboard {
            selected_id,
            confirm_remove,
            ..
        } => render_dashboard(
            &view.snapshots,
            state.accent_color,
            state.selection_color,
            selected_id.as_deref(),
            *confirm_remove,
            columns,
            rows,
        ),
        ProcessMode::Logs { id, scroll } => {
            render_logs(&view.manager, state.accent_color, id, scroll, columns, rows)
        }
    }
}

fn render_dashboard(
    snapshots: &[BackgroundSnapshot],
    accent_color: crate::config::UiColor,
    selection_color: crate::config::UiColor,
    selected_id: Option<&str>,
    confirm_remove: bool,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let now = Instant::now();
    let selected = selected_id.and_then(|id| {
        snapshots
            .iter()
            .position(|snapshot| snapshot.id.as_str() == id)
    });
    let active = snapshots
        .iter()
        .filter(|snapshot| snapshot.status.is_active())
        .count();
    let chrome = TableChrome {
        title: "Background terminals",
        summary: super::dashboard::summary(active, snapshots.len(), "running"),
        empty: "No tracked background terminals.",
        hint: if confirm_remove {
            "Remove selected process and its logs? Enter confirms, Esc keeps it"
        } else {
            "↑↓ move · Enter logs · x/Ctrl+C stop · r restart · d remove · Esc close"
        },
        accent: accent_color,
        selection: selection_color,
    };
    let cell = |snapshot: &BackgroundSnapshot, field, selected: bool| match field {
        ProcessField::Status => format!(
            "{} {} {}",
            if selected { "›" } else { " " },
            status_square(&snapshot.status),
            snapshot.status.label()
        ),
        ProcessField::Name => plain_preview(snapshot.display_name()),
        ProcessField::Id => snapshot.id.to_string(),
        ProcessField::Pid => snapshot
            .pid
            .map_or_else(|| "?".into(), |pid| pid.to_string()),
        ProcessField::Elapsed => format_duration(snapshot.elapsed(now)),
        ProcessField::Command => plain_preview(&snapshot.command),
    };
    let frame = render_table(
        snapshots,
        selected,
        chrome,
        process_columns,
        cell,
        columns,
        rows,
    );
    (frame, HIDDEN_CURSOR)
}

#[derive(Clone, Copy)]
enum ProcessField {
    Status,
    Name,
    Id,
    Pid,
    Elapsed,
    Command,
}

type ProcessColumn = FieldColumn<ProcessField>;

fn process_columns(width: usize) -> Vec<ProcessColumn> {
    if width >= 63 {
        let extra = width - 63;
        let name_extra = extra.min(10);
        let command_extra = extra - name_extra;
        vec![
            ProcessColumn::new(ProcessField::Status, "    STATUS", 13, Alignment::Left),
            ProcessColumn::new(ProcessField::Name, "NAME", 14 + name_extra, Alignment::Left),
            ProcessColumn::new(ProcessField::Id, "ID", 6, Alignment::Left),
            ProcessColumn::new(ProcessField::Pid, "PID", 6, Alignment::Right),
            ProcessColumn::new(ProcessField::Elapsed, "TIME", 7, Alignment::Right),
            ProcessColumn::new(
                ProcessField::Command,
                "COMMAND",
                12 + command_extra,
                Alignment::Left,
            ),
        ]
    } else if width >= 52 {
        let extra = width - 52;
        let name_extra = extra.min(6);
        let command_extra = extra - name_extra;
        vec![
            ProcessColumn::new(ProcessField::Status, "    STATUS", 13, Alignment::Left),
            ProcessColumn::new(ProcessField::Name, "NAME", 12 + name_extra, Alignment::Left),
            ProcessColumn::new(ProcessField::Id, "ID", 6, Alignment::Left),
            ProcessColumn::new(ProcessField::Elapsed, "TIME", 7, Alignment::Right),
            ProcessColumn::new(
                ProcessField::Command,
                "COMMAND",
                10 + command_extra,
                Alignment::Left,
            ),
        ]
    } else if width >= 31 {
        vec![
            ProcessColumn::new(ProcessField::Status, "    STATUS", 13, Alignment::Left),
            ProcessColumn::new(ProcessField::Name, "NAME", width - 21, Alignment::Left),
            ProcessColumn::new(ProcessField::Id, "ID", 6, Alignment::Left),
        ]
    } else {
        let status_width = 12.min(width.saturating_sub(2));
        vec![
            ProcessColumn::new(
                ProcessField::Status,
                "    STATUS",
                status_width,
                Alignment::Left,
            ),
            ProcessColumn::new(
                ProcessField::Name,
                "NAME",
                width.saturating_sub(status_width + 1),
                Alignment::Left,
            ),
        ]
    }
}

fn render_logs(
    manager: &BackgroundProcessManager,
    accent_color: crate::config::UiColor,
    id: &str,
    scroll: &mut PinnedScroll,
    columns: usize,
    rows: usize,
) -> (Vec<String>, (usize, usize)) {
    let columns = columns.max(20);
    let rows = rows.max(8);
    let Ok(detail) = manager.detail(id) else {
        return (
            vec![markdown::fit_width(
                &format!("Process {id} is no longer tracked. Press Esc."),
                columns,
            )],
            HIDDEN_CURSOR,
        );
    };
    let snapshot = &detail.snapshot;
    let pid = snapshot
        .pid
        .map_or_else(|| "?".into(), |pid| pid.to_string());
    let timeout = snapshot.timeout.map_or_else(
        || "no timeout".into(),
        |timeout| format!("{}s timeout", timeout.as_secs()),
    );
    let header = format!(
        "{} [{}] {}  pid {}  {}  {}",
        snapshot.id,
        snapshot.status.detail(),
        plain_preview(snapshot.display_name()),
        pid,
        format_duration(snapshot.elapsed(Instant::now())),
        timeout
    );
    let mut content = Vec::new();
    content.extend(markdown::plain_lines(
        &format!("$ {}\ncwd: {}", snapshot.command, snapshot.cwd.display()),
        columns,
    ));
    content.push(String::new());
    if detail.oldest_cursor > 0 {
        content.push("\x1b[2m[earlier output discarded]\x1b[0m".into());
    }
    let mut runs: Vec<(OutputStream, String)> = Vec::new();
    for chunk in detail.logs {
        if let Some((_, text)) = runs
            .last_mut()
            .filter(|(stream, _)| *stream == chunk.stream)
        {
            text.push_str(&chunk.text);
        } else {
            runs.push((chunk.stream, chunk.text));
        }
    }
    for (stream, text) in runs {
        let label = match stream {
            OutputStream::Stdout => "\x1b[2mstdout\x1b[0m",
            OutputStream::Stderr => "\x1b[2mstderr\x1b[0m",
        };
        content.push(label.into());
        content.extend(markdown::plain_lines(&text, columns));
    }
    if detail.next_cursor == 0 {
        content.push("\x1b[2mWaiting for output…\x1b[0m".into());
    }
    let height = rows.saturating_sub(2);
    let top = scroll.place(content.len(), height);
    let end = top.saturating_add(height).min(content.len());
    let mut frame = vec![markdown::fit_width(
        &format!("\x1b[1m{header}\x1b[0m"),
        columns,
    )];
    frame.extend(
        content[top..end]
            .iter()
            .map(|line| markdown::fit_width(line, columns)),
    );
    frame.extend(std::iter::repeat_n(
        " ".repeat(columns),
        rows.saturating_sub(frame.len() + 1),
    ));
    let hint = "↑/↓ or PgUp/PgDn scroll  x or Ctrl+C stop  Esc dashboard";
    frame.push(format!(
        "{}{}\x1b[0m",
        status_style(accent_color),
        markdown::fit_width(&format!(" {hint}"), columns)
    ));
    (frame, HIDDEN_CURSOR)
}

fn status_square(status: &BackgroundStatus) -> &'static str {
    match status {
        BackgroundStatus::Starting | BackgroundStatus::Running => "\x1b[32m■\x1b[0m",
        BackgroundStatus::Stopping => "\x1b[33m■\x1b[0m",
        BackgroundStatus::Exited(0) => "\x1b[36m■\x1b[0m",
        BackgroundStatus::Exited(_)
        | BackgroundStatus::Signaled
        | BackgroundStatus::TimedOut
        | BackgroundStatus::Failed(_) => "\x1b[31m■\x1b[0m",
        BackgroundStatus::Stopped => "\x1b[2m■\x1b[0m",
    }
}

fn plain_preview(text: &str) -> String {
    markdown::plain_lines(text, 4096).join(" ")
}

#[cfg(test)]
mod tests {

    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::background::{BackgroundId, StartSpec};
    use crate::config::UiColor;

    fn snapshot(status: BackgroundStatus) -> BackgroundSnapshot {
        BackgroundSnapshot {
            id: BackgroundId::new(1),
            pid: Some(123),
            command: "printf '\x1b[31muntrusted'".into(),
            name: Some("dev\x1b[2Jserver".into()),
            cwd: PathBuf::from("/tmp/project"),
            timeout: None,
            status,
            started_at: Instant::now() - Duration::from_secs(5),
            settled_at: None,
        }
    }

    fn process_state(manager: BackgroundProcessManager) -> ViewState {
        ViewState {
            background_processes: manager,
            background_active_count: 1,
            ..ViewState::test_default()
        }
    }

    fn running_dashboard() -> (ViewState, BackgroundProcessManager, BackgroundId) {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(StartSpec {
                command: "sleep 30".into(),
                name: Some("test server".into()),
                cwd: std::env::current_dir().expect("test working directory"),
                timeout: None,
            })
            .expect("start background process");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let running = manager.snapshots().into_iter().any(|snapshot| {
                snapshot.id == started.id && snapshot.status == BackgroundStatus::Running
            });
            if running {
                break;
            }
            assert!(Instant::now() < deadline, "process did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut state = process_state(manager.clone());
        open_dashboard(&mut state, manager.clone());
        (state, manager, started.id)
    }

    #[test]
    fn stop_shortcuts_request_stop_without_confirmation() {
        for key in [Key::Char('x'), Key::Ctrl('c')] {
            let (mut state, manager, id) = running_dashboard();
            match key {
                Key::Ctrl('c') => assert!(handle_interrupt(&mut state)),
                key => handle_event(&mut state, Event::Key(key)),
            }
            let status = manager
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.id == id)
                .expect("tracked process")
                .status;
            let has_confirmation = matches!(
                state.process_view.as_ref().map(|view| &view.mode),
                Some(ProcessMode::Dashboard {
                    confirm_remove: true,
                    ..
                })
            );
            manager.shutdown_and_discard();

            assert_eq!(status, BackgroundStatus::Stopping);
            assert!(!has_confirmation);
        }
    }

    #[test]
    fn log_stop_shortcuts_request_stop_without_leaving_logs() {
        for key in [Key::Char('x'), Key::Ctrl('c')] {
            let (mut state, manager, id) = running_dashboard();
            handle_event(&mut state, Event::Key(Key::Enter));
            match key {
                Key::Ctrl('c') => assert!(handle_interrupt(&mut state)),
                key => handle_event(&mut state, Event::Key(key)),
            }
            let status = manager
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.id == id)
                .expect("tracked process")
                .status;
            let still_showing_logs = matches!(
                state.process_view.as_ref().map(|view| &view.mode),
                Some(ProcessMode::Logs { id: selected, .. }) if selected == id.as_str()
            );
            manager.shutdown_and_discard();

            assert_eq!(status, BackgroundStatus::Stopping);
            assert!(still_showing_logs);
        }
    }

    #[test]
    fn dashboard_fits_narrow_terminals_and_sanitizes_metadata() {
        let (frame, cursor) = render_dashboard(
            &[snapshot(BackgroundStatus::Running)],
            UiColor::WHITE,
            UiColor::WHITE,
            Some("bg-1"),
            false,
            20,
            8,
        );
        assert_eq!(frame.len(), 8);
        assert_eq!(cursor, HIDDEN_CURSOR);
        assert!(frame.iter().all(|line| markdown::visible_width(line) <= 20));
        assert!(!frame.join("\n").contains("\x1b[2J"));
    }

    #[test]
    fn dashboard_describes_remove_confirmation() {
        let snapshots = [snapshot(BackgroundStatus::Stopped)];
        let (remove, _) = render_dashboard(
            &snapshots,
            UiColor::WHITE,
            UiColor::WHITE,
            Some("bg-1"),
            true,
            80,
            10,
        );
        assert!(
            markdown::strip_ansi(&remove.join("\n"))
                .contains("Remove selected process and its logs?")
        );
    }

    #[test]
    fn successful_stderr_progress_is_neutral_and_replays_terminal_updates() {
        let manager = BackgroundProcessManager::default();
        let started = manager
            .start(StartSpec {
                command: "printf 'Compiling alpha\\nBuilding [>   ] 1/2\\rBuilding [==> ] 2/2\\nFinished\\n' >&2".into(),
                name: Some("release build".into()),
                cwd: std::env::current_dir().expect("test working directory"),
                timeout: None,
            })
            .expect("start background process");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let settled = manager
                .snapshots()
                .into_iter()
                .find(|snapshot| snapshot.id == started.id)
                .is_some_and(|snapshot| !snapshot.status.is_active());
            if settled {
                break;
            }
            assert!(Instant::now() < deadline, "process did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (frame, _) = render_logs(
            &manager,
            UiColor::WHITE,
            started.id.as_str(),
            &mut PinnedScroll::default(),
            100,
            20,
        );
        manager.shutdown_and_discard();
        let plain = frame
            .iter()
            .map(|line| markdown::strip_ansi(line).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let stderr = plain
            .split_once("\nstderr\n")
            .map(|(_, stderr)| stderr)
            .expect("rendered stderr section");

        assert!(stderr.contains("Compiling alpha\nBuilding [==> ] 2/2\nFinished"));
        assert!(!stderr.contains("Building [>   ] 1/2"));
        assert!(!stderr.contains('�'));
        assert!(!frame.iter().any(|line| line.contains("\x1b[31m")));
    }

    #[test]
    fn dashboard_is_compact_and_uses_the_same_column_grid_for_headers_and_data() {
        let (frame, _) = render_dashboard(
            &[snapshot(BackgroundStatus::Running)],
            UiColor::WHITE,
            UiColor::WHITE,
            Some("bg-1"),
            false,
            100,
            24,
        );
        let plain = frame
            .iter()
            .map(|line| markdown::strip_ansi(line))
            .collect::<Vec<_>>();
        let header = plain
            .iter()
            .find(|line| line.contains("STATUS") && line.contains("COMMAND"))
            .expect("wide dashboard should show the full table header");
        let data = plain
            .iter()
            .find(|line| line.contains("bg-1"))
            .expect("the tracked background terminal should be visible");
        let column = |line: &str, value: &str| {
            let index = line.find(value).expect("fixture value should be visible");
            markdown::visible_width(&line[..index])
        };
        let right_edge =
            |line: &str, value: &str| column(line, value) + markdown::visible_width(value);

        assert_eq!(column(header, "STATUS"), column(data, "running"));
        assert_eq!(column(header, "NAME"), column(data, "devserver"));
        assert_eq!(column(header, "ID"), column(data, "bg-1"));
        assert_eq!(right_edge(header, "PID"), right_edge(data, "123"));
        assert_eq!(column(header, "COMMAND"), column(data, "printf"));

        let occupied = plain
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        assert_eq!(occupied.len(), 9);
        assert!(occupied[0] > 0);
        assert!(occupied[8] < frame.len() - 1);
    }
}
